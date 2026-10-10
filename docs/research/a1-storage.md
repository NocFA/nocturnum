# 1. Summary

Source citations beginning `lib/` or `config/` are relative to the root of the `qdrant/qdrant` repository at package version `1.19.2`, documentation and article citations are relative to `qdrant-landing/content` in the `qdrant/landing_page` repository.
This report describes the supplied source tree, with no builds, tests or runtime verification performed (`Cargo.toml:3`).

- **The persistence boundary is the local shard**, each collection contains shard replica sets, each local shard owns its WAL, segment holder and update workers, while each segment owns its IDs, vectors, payloads and indexes (`lib/collection/src/collection/mod.rs:69`, `lib/collection/src/shards/local_shard/mod.rs:111`, `lib/segment/src/segment/mod.rs:65`).
- **Indexed segments are replaced through background rebuilds**, ordinary insertions enter appendable segments, changes to points in non-appendable segments use copy-on-write, and proxies keep queries and writes flowing while optimization or snapshotting freezes the original data (`lib/segment/src/types.rs:2173`, `lib/shard/src/segment_holder/mod.rs:1080`, `lib/shard/src/optimize.rs:652`).
- **Durability depends on explicit flushes and ordered publication**, the WAL append itself writes mapped memory, callback-bearing updates flush the WAL before applying, and segment persistence advances only after mappings, data, indexes and point versions have flushed in the required order (`lib/wal/src/segment.rs:331`, `lib/collection/src/update_workers/update_worker.rs:346`, `lib/segment/src/segment/entry.rs:400`).
- **RocksDB has already disappeared from the inspected storage paths**, current payload and sparse storage use `Blobstore`, mutable mode uses Gridstore, append-only mode uses Logstore, and loading the old dense `Memory` format returns an unsupported-RocksDB error (`lib/blobstore/src/blobstore/mod.rs:34`, `lib/segment/src/segment_constructor/segment_constructor_base/vector_storage.rs:112`).
- **nocturnum should copy the recovery invariants conceptually**, especially deterministic operation ordering, version-gated replay, destination-before-source persistence and durable replacement publication, while a minimal v1 should reduce storage modes, proxy layers and visibility states substantially **(inferred)** from the complexity concentrated in `lib/shard/src/segment_holder/flush.rs:38`, `lib/shard/src/optimize.rs:415` and `lib/segment/src/pending_changes/mod.rs:1`.

# 2. How it works, fundamentally

## Collection → shard → segment

`Collection` owns configuration and a shared `ShardHolder`, with its data split across shard replica sets.
`LocalShard` is the single-node engine boundary, containing a `LockedSegmentHolder`, `RecoverableWal`, `UpdateHandler`, bounded update sender, optimizer configuration and applied-operation tracking (`lib/collection/src/collection/mod.rs:69`, `lib/collection/src/shards/local_shard/mod.rs:111`).

The current implementation separates reusable engine machinery into `lib/shard`, older-looking optimizer files under `lib/collection` can be compatibility wrappers.
For example, the collection optimizer module re-exports the shard implementation, so tracing only collection code misses the actual replacement and persistence logic (`lib/collection/src/collection_manager/optimizers/segment_optimizer.rs:1`).

`SegmentHolder` divides segments into deterministic `BTreeMap`s of appendable and non-appendable entries, with virtual segment IDs independent of segment UUIDs.
It also tracks failed operations, optimizer errors, copy-on-write flush dependencies, pending post-flush actions and the background flush thread (`lib/shard/src/segment_holder/mod.rs:87`).

`LockedSegment` is either `Original(Arc<RwLock<Segment>>)` or `Proxy(Arc<RwLock<ProxySegment>>)`.
A `Segment` contains an ID tracker, named vector stores and indexes, payload storage, a structured payload index, operation versions and persisted-version tracking (`lib/shard/src/locked_segment.rs:32`, `lib/segment/src/segment/mod.rs:65`).

Shard files include `wal/`, `segments/`, clock maps and `applied_seq.json`, with an optional `segments_manifest.json` beside the segment directory.
Segment state is stored in `segment.json`, segment directories use UUIDs, and `version.info` acts as a readiness marker, directories without that marker are removed during normalization (`lib/shard/src/files/mod.rs:5`, `lib/segment/src/segment/mod.rs:40`, `lib/segment/src/segment_constructor/segment_constructor_base/segment.rs:50`).

## Appendable and non-appendable segments

Appendability requires every configured vector family to support appending.
For dense vectors, the index must be plain and storage must be appendable, HNSW makes the vector configuration non-appendable even when its underlying vector store supports appends, while sparse index type contributes independently to the segment-wide decision (`lib/segment/src/types.rs:1781`, `lib/segment/src/types.rs:2173`).

Non-appendable therefore describes insertion capability, the segment still supports deletions and several metadata changes.
The documentation’s description of non-appendable segments as supporting only reads and deletes is incomplete, source also permits payload-index and named-vector schema changes, tracked separately for snapshots (`documentation/manage-data/storage.md:21`, `lib/segment/src/segment/entry.rs:606`, `lib/segment/src/segment/version_tracker.rs:9`).

When changing a point in a non-appendable segment or proxy, `SegmentHolder` retrieves its latest record, writes the changed record into an appendable destination, then removes or shadows the source.
It records a flush dependency requiring the destination to become durable before source deletion can persist, preserving a recoverable pre-image if the process crashes halfway through (`lib/shard/src/segment_holder/mod.rs:1080`).

The holder checks existing point versions before applying these moves, source records can temporarily overlap across segments, and optimizer merging resolves duplicates by version.
This resembles immutable search segments plus a mutable write layer **(inferred)**, rather than a single globally mutable index (`lib/shard/src/segment_holder/mod.rs:1100`, `lib/segment/src/id_tracker/mod.rs:47`).

## Operation ordering and update queues

Collection updates are split by shard and dispatched through `FuturesUnordered`, each local shard assigns its own WAL sequence numbers.
A collection with several local shards therefore has several operation streams, the inspected path does not establish a collection-wide atomic transaction across them **(inferred)** (`lib/collection/src/collection/point_ops.rs:161`).

Ordinary local submission follows this sequence:

1. Check WAL disk headroom and acquire the submission read fence (`lib/collection/src/shards/local_shard/shard_ops.rs:64`).
2. Reserve space in the bounded update channel before appending (`lib/collection/src/shards/local_shard/shard_ops.rs:121`).
3. Append under the WAL mutex, obtaining the operation sequence number (`lib/collection/src/wal_delta.rs:50`).
4. Send the operation while retaining that mutex, ensuring channel order matches WAL order (`lib/collection/src/shards/local_shard/shard_ops.rs:123`).
5. Return immediately or await the operation’s callback according to `WaitUntil` (`lib/collection/src/shards/local_shard/shard_ops.rs:165`).

The default queue holds 200 operations, while up to 500 pending operation bodies may be kept in RAM.
Beyond that body threshold, queue entries retain sequence numbers and the worker reloads serialized operations from the WAL, the default queue is smaller than this threshold, so that overflow mechanism matters when queue capacity is increased (`lib/collection/src/operations/shared_storage_config.rs:16`, `lib/collection/src/shards/local_shard/shard_ops.rs:139`, `lib/collection/src/update_workers/update_worker.rs:94`).

Backpressure starts before WAL growth because submission awaits a channel reservation, though capacity is measured in operations rather than serialized bytes.
A very large operation can still consume substantial memory and disk **(inferred)** from that admission mechanism (`lib/collection/src/shards/local_shard/shard_ops.rs:121`).

The update worker processes operations serially, moving blocking work into `spawn_blocking`.
`CollectionUpdater` additionally takes the update-operation write lock and the holder’s updates lock, coordinating application with optimizer finalization and internal scroll/retrieve critical sections (`lib/collection/src/update_workers/update_worker.rs:60`, `lib/collection/src/collection_manager/collection_updater.rs:40`).

Filter-based mutations receive special treatment, submission takes an exclusive fence, drains preceding updates with a `Plunger`, resolves the predicate to concrete IDs, and appends the rewritten operation.
The WAL mutex stays unlocked during the drain to avoid deadlocking workers that need it, and the resolved operation remains one WAL record regardless of matched point count (`lib/collection/src/shards/local_shard/resolve_submit.rs:1`, `lib/collection/src/shards/local_shard/resolve_submit.rs:68`).

This makes replay reproduce the original target set even if indexes or payload state have changed, while large predicate matches can produce very large records **(inferred)** from the explicitly unsplit implementation (`lib/collection/src/shards/local_shard/resolve_submit.rs:24`).

## WAL format, recovery and truncation

`SerdeWal<R>` wraps the byte-oriented `Wal`, serializing operations as CBOR.
The shard stores `OperationWithClockTag`, combining the mutation with replication ordering metadata, clock rejection can prevent an operation from being queued (`lib/shard/src/wal.rs:20`, `lib/shard/src/wal.rs:40`, `lib/collection/src/wal_delta.rs:50`).

`Wal` maintains one open segment, closed segments and a segment creator, with an exclusive filesystem lock preventing concurrent ownership.
Files use `open-*` and `closed-*` naming, segment rotation starts an asynchronous flush of the retired segment, and oversized entries can expand capacity (`lib/wal/src/lib.rs:76`, `lib/wal/src/lib.rs:154`, `lib/wal/src/lib.rs:264`, `lib/wal/src/lib.rs:294`).

The physical format is explicit and little-endian:

| Structure | Encoding |
|---|---|
| Segment header | Three-byte `wal` magic, one-byte format version, four-byte random CRC seed |
| Record header | Eight-byte payload length |
| Record body | Serialized bytes followed by alignment padding |
| Record footer | Four-byte CRC32C, with complete entries aligned to eight bytes |

The CRC chain starts from the random segment seed, which also prevents stale entries in reused files from being accepted (`lib/wal/src/segment.rs:19`, `lib/wal/src/segment.rs:48`).

Opening scans records and rebuilds an in-memory offset index, parsing stops at the first invalid CRC or incomplete record.
The remainder is treated as empty, so a corrupt middle record can hide subsequent records even if their bytes remain present **(inferred)** from the scanner (`lib/wal/src/segment.rs:243`).

Recovery loads segment components and any persisted proxy-change logs before ordinary WAL replay.
It computes a synchronous replay range using the WAL boundaries, applied-sequence estimate and bounded queue capacity, replays that range, force-flushes segments, then queues any remaining tail for the normal update worker (`lib/collection/src/shards/local_shard/mod.rs:464`, `lib/collection/src/shards/local_shard/mod.rs:768`, `lib/collection/src/shards/local_shard/mod.rs:810`, `lib/collection/src/shards/local_shard/mod.rs:979`, `lib/collection/src/shards/local_shard/mod.rs:997`).

`AppliedSeqHandler` persists every 64 updates, its in-memory value is precise while its file may lag.
Recovery clamps against the WAL’s logical first index because successful segment flushes and truncation can move that boundary beyond the older applied-sequence file (`lib/collection/src/update_workers/applied_seq.rs:14`, `lib/collection/src/update_workers/applied_seq.rs:54`, `lib/collection/src/shards/local_shard/mod.rs:818`).

Service errors and out-of-memory errors abort synchronous recovery, other operation errors are logged and skipped.
Historical vector names are reconciled against the current schema before replay, rather than allowing removed names to invalidate the whole historical mutation (`lib/collection/src/shards/local_shard/mod.rs:873`, `lib/collection/src/shards/local_shard/mod.rs:919`).

Truncation has two layers, `Wal::prefix_truncate` removes whole closed files, while `SerdeWal` records a finer logical boundary in `first-index`.
Acknowledged bytes may remain physically present, at least the configured closed-file retention is preserved, and the boundary entry may be replayed again (`lib/wal/src/lib.rs:410`, `lib/shard/src/wal.rs:189`, `lib/shard/src/wal.rs:216`).

Live WAL pins cap acknowledgement below the lowest pinned operation, snapshots and transfer queues use these pins.
Holder-level post-flush actions also constrain acknowledgement until retired data or recovered proxy logs can safely be removed (`lib/collection/src/update_workers/flush_workers.rs:21`, `lib/shard/src/segment_holder/mod.rs:47`).

## Point versions and ID trackers

External IDs are `ExtendedPointId::NumId(u64)` or `Uuid(Uuid)`, internal IDs are segment-local `u32` offsets and operation sequence numbers are `u64`.
Storage and indexes operate on compact internal offsets, an external ID can have different offsets in different segment copies (`lib/segment/src/types.rs:60`, `lib/segment/src/types.rs:175`, `lib/common/common/src/types.rs:10`).

Version checks exist at several levels.
The segment handler rejects operations older than a point’s version, while the holder’s copy-on-write path skips equal or newer versions, equal-version replay is therefore permitted in some lower-level paths and relies on idempotent mutation behavior (`lib/segment/src/segment/segment_ops.rs:621`, `lib/shard/src/segment_holder/mod.rs:1100`).

`IdTracker` and its read-side interfaces separate lookup, version access, deletion state and persistence.
The inspected tree contains three persistent tracker formats (`lib/segment/src/id_tracker/mod.rs:1`, `lib/segment/src/id_tracker/format_detection.rs:14`):

| Tracker | Runtime and persistence behavior |
|---|---|
| `MutableIdTracker` | RAM mappings and versions, persisted through mapping changes and a dense version file |
| `ImmutableIdTracker` | Compressed RAM mappings and versions, persistent deletion/version files remain writable |
| `DiskIdTracker` | Immutable mapping on disk, resident deletion bits, compressed versions and sparse lookup metadata |

Mutable mappings use separate numeric and UUID `BTreeMap`s plus an internal-to-external vector and deletion bits.
With deferred points, active and deferred heads occupy separate maps, the old active head can remain visible while the latest mutation is deferred, and optimizer-facing iteration prefers the deferred head (`lib/segment/src/id_tracker/point_mappings.rs:27`, `lib/segment/src/id_tracker/point_mappings.rs:319`, `lib/segment/src/id_tracker/point_mappings.rs:494`).

`mutable_id_tracker.mappings` contains concatenated tagged mapping changes, while `mutable_id_tracker.versions` stores one little-endian `u64` per internal offset.
The mapping history grows until segment optimization rewrites it, partial trailing version entries are ignored on load and healed before subsequent writes (`lib/segment/src/id_tracker/mutable_id_tracker/mod.rs:39`, `lib/segment/src/id_tracker/mutable_id_tracker/mappings_storage.rs:103`, `lib/segment/src/id_tracker/mutable_id_tracker/versions_storage.rs:1`).

The disk tracker stores an internal-to-external `u128` array, a UUID-discriminator bitmask, and sorted numeric/UUID external-to-internal runs.
An in-RAM sparse index selects roughly 16 KiB data blocks, mappings stay on disk while deletion bits and versions remain resident (`lib/segment/src/id_tracker/disk_id_tracker/on_disk_format.rs:1`, `lib/segment/src/id_tracker/disk_id_tracker/mod.rs:39`).

Optimization performs a sorted multiway merge over IDs, retaining the highest-version copy and excluding a winning tombstone.
Entries without a point version are discarded because they indicate interrupted persistence (`lib/segment/src/id_tracker/mod.rs:47`).

## Vector storage

The current production path distinguishes persistence layout from memory placement, cold and cached data can share the same disk format.
Old `Memory` dense storage survives as an enum variant for compatibility but fails to load, volatile RAM implementations are explicitly intended for temporary testing (`lib/segment/src/segment_constructor/segment_constructor_base/vector_storage.rs:112`, `lib/segment/src/vector_storage/dense/volatile_dense_vector_storage.rs:24`).

**Dense appendable storage** uses `AppendableMmapDenseVectorStorage<T>`, backed by `ChunkedVectors` and separate deletion flags.
Its directory contains configuration, a vector-count status file and `chunk_<n>.mmap` files, individual vectors never cross chunk boundaries, production chunks target 32 MiB and round down to whole vectors (`lib/segment/src/vector_storage/dense/appendable_dense_vector_storage.rs:29`, `lib/segment/src/vector_storage/chunked_vectors/mod.rs:1`, `lib/segment/src/vector_storage/common.rs:29`, `lib/segment/src/vector_storage/chunked_vectors/config.rs:148`).

**Dense immutable storage** uses a contiguous vector blob with a four-byte `data` header followed by fixed-width vector records, deletion storage uses a separate `drop` header.
The same read structure can use mmap or another universal I/O backend, vector count is derived from file length and configured element width (`lib/segment/src/vector_storage/dense/immutable_dense_vectors.rs:25`).

The storage enum supports float32, uint8 and float16 families, Turbo representations, graph-inline vectors, appendable stores and Linux io_uring variants.
Graph-inline storage places vectors in HNSW link storage rather than a dedicated vector file (`lib/segment/src/vector_storage/vector_storage_base.rs:622`, `lib/segment/src/segment_constructor/segment_constructor_base/vector_storage.rs:147`).

**Multivectors** use flattened chunked vectors plus an offsets structure, each `MultivectorMmapOffset` contains `offset`, `count` and `capacity`.
The constructor explicitly uses appendable multivector storage even when opening the nominal immutable mmap configuration, segment appendability still depends on the index (`lib/segment/src/vector_storage/multi_dense/appendable_mmap_multi_dense_vector_storage.rs:42`, `lib/segment/src/segment_constructor/segment_constructor_base/vector_storage.rs:27`).

**Sparse vectors** use `MmapSparseVectorStorage`, containing `Blobstore<StoredSparseVector>` and deletion flags.
Stored indices are sorted and delta-packed, values accompany them in a bincode representation, blob-level LZ4 is disabled because sparse indices already have specialized compression, and raw sparse storage is left unpopulated because index search does not read those values directly (`lib/segment/src/vector_storage/sparse/mmap_sparse_vector_storage.rs:31`, `lib/segment/src/vector_storage/sparse/mmap_sparse_vector_storage.rs:59`, `lib/segment/src/vector_storage/sparse/mmap_sparse_vector_storage.rs:97`, `lib/segment/src/vector_storage/sparse/stored_sparse_vectors.rs:10`).

**io_uring selection** happens when a component opens.
`storage.performance.io_uring=auto` chooses supported cold components when their feature gate permits it, absent configuration preserves legacy component-specific behavior, payload storage falls back to mmap if opening its io_uring variant fails (`lib/segment/src/common/io_uring.rs:14`, `lib/segment/src/common/io_uring.rs:88`, `lib/segment/src/segment_constructor/segment_constructor_base/payload_storage.rs:21`).

## Payload storage and the removal of RocksDB

`PayloadStorageImpl` serializes payloads as JSON bytes inside `Blobstore`.
Production variants are mmap and Linux io_uring, the enum’s separate in-memory implementation is gated by the testing feature (`lib/segment/src/payload_storage/payload_storage_impl.rs:19`, `lib/segment/src/payload_storage/payload_storage_enum.rs:20`).

`Blobstore` supports mutable **Gridstore** and append-only **Logstore** modes.
Creation selects the mode through feature flags, existing stores reopen according to persisted configuration, normal defaults select mutable storage (`lib/blobstore/src/blobstore/mod.rs:34`, `lib/segment/src/common/blobstore_config.rs:1`, `lib/common/common/src/flags.rs:104`).

Gridstore uses a direct point-offset tracker pointing into page files, a bitmask marking allocated blocks, and region gap summaries accelerating free-space searches.
Defaults are 128-byte blocks, 32 MiB pages, 8,192 blocks per region and LZ4 compression, `ValuePointer` stores page ID, block offset and encoded byte length (`lib/blobstore/src/blobstore/gridstore/mod.rs:33`, `lib/blobstore/src/config.rs:10`, `lib/blobstore/src/tracker/mod.rs:104`).

Updates allocate fresh blocks and buffer pointer changes, keeping old blocks unavailable until replacement pointers are durable.
The flush sequence persists allocation state, data pages, journaled tracker updates and finally freed old blocks, tracker journaling repairs torn pointer writes (`lib/blobstore/src/blobstore/gridstore/mod.rs:300`, `lib/blobstore/src/blobstore/gridstore/mod.rs:608`).

Logstore packs values consecutively into page files, requires monotonically increasing point offsets and forbids overwrite/delete.
Append-only segment mutations therefore clone records and tombstone old IDs, leaving underlying payload rows and postings untouched (`lib/blobstore/src/config.rs:109`, `lib/segment/src/segment/mod.rs:91`, `lib/segment/src/segment/segment_ops.rs:661`).

The official Gridstore article attributes the RocksDB replacement to LSM compaction latency spikes, generic-key overhead, tuning complexity and C++ interoperability, Qdrant already controlled recovery and segment compaction.
It dates initial payload/sparse adoption to 1.13 and the default for new deployments to 1.15 (`articles/gridstore-key-value-storage.md:17`, `articles/gridstore-key-value-storage.md:246`).

Its statement that RocksDB would remain supported is historical, this source rejects the old dense format and discards deprecated RocksDB backup directories during snapshot restoration.
An ancient archive containing those backups is therefore not evidence of usable recovery compatibility (`articles/gridstore-key-value-storage.md:23`, `lib/segment/src/segment_constructor/segment_constructor_base/vector_storage.rs:112`, `lib/segment/src/segment/segment_ops.rs:1015`).

## Optimizers and uninterrupted writes

`SegmentOptimizer` provides shared planning and rebuilding behavior, concrete implementations cover vacuum, merge, indexing and configuration mismatch.
`OptimizationPlanner` removes planned inputs from its remaining set so one plan does not assign the same segment repeatedly (`lib/shard/src/optimizers/segment_optimizer.rs:81`, `lib/shard/src/optimizers/segment_optimizer.rs:420`, `lib/shard/src/optimizers/segment_optimizer.rs:480`).

Vacuum selects heavily deleted segments or indexes, merge greedily batches smaller segments below the size cap, and indexing selects segments exceeding indexing or storage-placement thresholds.
Merge explicitly avoids transformations that merely replace two inputs with one output plus a new write segment, preserving the original segment count (`lib/shard/src/optimizers/vacuum_optimizer.rs:59`, `lib/shard/src/optimizers/merge_optimizer.rs:17`, `lib/shard/src/optimizers/indexing_optimizer.rs:53`).

The replacement lifecycle is:

1. Validate inputs and estimate temporary disk needs, creating an extra appendable destination if necessary (`lib/shard/src/optimize.rs:652`).
2. Install proxies under the holder write lock, finalizing deletion masks only after source segments are frozen (`lib/shard/src/optimize.rs:741`).
3. Copy, merge and build indexes outside the long critical section, collecting concurrent proxy changes (`lib/shard/src/optimize.rs:195`).
4. Block updates briefly, propagate remaining changes, force-flush the result and publish its readiness marker (`lib/shard/src/optimize.rs:415`).
5. Swap membership and defer destruction of old data until a durable flush boundary permits it, with acknowledgement pins registered under the same lock (`lib/shard/src/optimize.rs:459`).

`ProxySegment` buffers deletions and schema changes while record mutations route to appendable destinations.
Optional persisted proxy logs allow acknowledgement to advance without retaining all corresponding WAL history, that feature defaults off, ordinary proxies report the wrapped segment’s persisted version (`lib/shard/src/proxy_segment/mod.rs:21`, `lib/shard/src/proxy_segment/segment_entry.rs:837`, `lib/common/common/src/flags.rs:104`).

Cancellation propagates buffered changes before unwrapping, failed propagation leaves proxies installed.
Partially built outputs are cleaned up best-effort, a process crash can leave an orphan that startup must reclaim (`lib/shard/src/optimize.rs:87`, `lib/shard/src/optimize.rs:121`).

## Flush guarantees and snapshots

`WaitUntil::Wal` acknowledges after append, `Segment` waits for application and `Visible` additionally waits for deferred visibility.
Boolean `wait=true` maps to `Visible`, both callback-bearing modes cause the worker to flush the WAL before mutation, neither requires immediate flushing of every segment component (`lib/collection/src/shards/shard_trait.rs:23`, `lib/collection/src/update_workers/update_worker.rs:346`).

The documentation promises that an operation added to WAL survives power loss, but the low-level append explicitly requires a later flush.
The source supports a weaker append acknowledgement and a stronger callback-bearing path, so the unconditional documentation claim should not define nocturnum’s contract (`documentation/manage-data/storage.md:95`, `lib/wal/src/segment.rs:331`, `lib/collection/src/update_workers/update_worker.rs:358`).

Periodic flushing first flushes WAL, captures the last fully applied sequence, starts ordered segment flushing and acknowledges only the confirmed durable boundary.
Segment flush order is mappings → vectors/quantization → payload indexes and payload storage → point versions → segment state, with background persistence capped against partially applied operations (`lib/collection/src/update_workers/flush_workers.rs:67`, `lib/segment/src/segment/entry.rs:418`, `lib/segment/src/index/struct_payload_index/payload_index.rs:249`).

The holder acquires segment locks in non-appendable-before-appendable order to match copy-on-write locking, while actual flushing follows destination dependencies.
Capturing all flushers before releasing locks prevents an interleaved move from persisting source deletion without its destination (`lib/shard/src/segment_holder/flush.rs:38`).

Collection snapshots are tar archives containing shard snapshots, collection configuration/version, shard-key mappings and payload-index schema.
Regular segment snapshots contain nested `<uuid>.tar` archives, streamable snapshots use directories, and partial snapshots use file-version manifests to include changed files (`lib/collection/src/collection/snapshots.rs:56`, `lib/segment/src/segment/snapshot.rs:51`).

Snapshot creation proxies segments so updates continue, WAL-inclusive snapshots pin retention before segment copying and release it after archiving WAL.
WAL-less snapshots capture clocks before draining the queue, avoiding a recovery point newer than their segment data (`lib/collection/src/shards/local_shard/snapshot.rs:88`, `lib/collection/src/shards/local_shard/snapshot.rs:124`, `lib/collection/src/shards/local_shard/snapshot.rs:229`).

Restore extracts archives, restores segment layouts and loads the shard through normal recovery.
Partial restore also removes local tracker journals that could otherwise replay stale pointer changes over replacement files (`lib/shard/src/snapshots/snapshot_utils.rs:17`, `lib/shard/src/snapshots/snapshot_utils.rs:69`, `lib/segment/src/segment/segment_ops.rs:956`).

The local snapshot manager writes a SHA-256 checksum sidecar and promotes temporary files into place.
Snapshots cover node-local data, collection aliases are excluded, and the documentation supports restoration to the same minor version or the next minor version, those compatibility claims remain distinct from verifying a particular old storage format (`lib/collection/src/common/snapshots_manager.rs:454`, `documentation/snapshots.md:16`, `documentation/snapshots.md:56`).

# 3. Config knobs and defaults that matter

| Knob | Default and consequence | Evidence |
|---|---|---|
| Shard count | 1, increasing it creates separate local WAL/update streams | `lib/collection/src/config.rs:355` |
| `wal_capacity_mb` | 32 MiB per WAL segment, oversized records can expand storage | `lib/collection/src/config.rs:40`, `lib/wal/src/lib.rs:294` |
| `wal_segments_ahead` | 0, precreation can reduce rollover allocation latency | `lib/collection/src/config.rs:69` |
| `wal_retain_closed` | 1, logical acknowledgement does not remove every old byte | `lib/collection/src/config.rs:52` |
| Update queue | 200 operations, pending bodies remain in RAM below 500 | `lib/collection/src/operations/shared_storage_config.rs:16` |
| `flush_interval_sec` | 5 seconds, periodic worker interval rather than a strict acknowledgement guarantee | `config/config.yaml:209`, `lib/collection/src/update_workers/flush_workers.rs:138` |
| `deleted_threshold` | 0.2, vacuum comparisons use strictly greater | `lib/shard/src/optimizers/config.rs:18`, `lib/shard/src/optimizers/vacuum_optimizer.rs:59` |
| `vacuum_min_vector_number` | 1,000, point-level and vector-index-level checks use different counts | `lib/shard/src/optimizers/vacuum_optimizer.rs:59` |
| `default_segment_number` | 0 resolves to CPU count / 2, clamped to 2–8 | `lib/shard/src/optimizers/config.rs:190` |
| `max_segment_size` | Automatic: resolved indexing thread count × 256,000 KiB | `lib/shard/src/optimizers/config.rs:17`, `lib/shard/src/optimizers/config.rs:228` |
| `indexing_threshold` | 10,000 KiB, zero disables threshold-triggered indexing | `lib/shard/src/optimizers/config.rs:18`, `lib/shard/src/optimizers/config.rs:219` |
| `memmap_threshold` | Unset/zero disables automatic threshold conversion, deprecated since 1.15 | `lib/collection/src/optimizers_builder.rs:72`, `lib/collection/src/optimizers_builder.rs:146` |
| `max_optimization_threads` | Null chooses dynamically, zero disables optimization jobs | `lib/collection/src/optimizers_builder.rs:91` |
| `max_indexing_threads` | 0 resolves automatically, independent of per-shard job count | `lib/segment/src/index/hnsw_index/mod.rs:89` |
| Dense `memory` | Cached by default, cold avoids startup population | `lib/shard/src/optimizers/config.rs:59` |
| Payload `memory` | Cold by default, overrides deprecated `on_disk_payload` | `lib/collection/src/config.rs:174`, `lib/collection/src/config.rs:253` |
| ID tracker `memory` | Pinned normally, cold in serverless-compatible mode, affects non-appendable output | `lib/segment/src/types.rs:1740` |
| `io_uring` | Unset follows component fallback, `auto` selects eligible cold components | `lib/segment/src/common/io_uring.rs:88` |
| `prevent_unoptimized` | Disabled, enabling it defers visibility beyond configured thresholds | `lib/collection/src/optimizers_builder.rs:104` |
| Append-only/proxy persistence flags | Append-only mutations, append-only storage and persisted proxies default false | `lib/common/common/src/flags.rs:104` |

Thresholds measure vector bytes, the “1 KiB equals one 256-dimensional vector” explanation assumes float32.
They do not cap payload bytes or complete segment filesystem size, optimizer disk admission separately estimates actual source directory sizes (`lib/collection/src/optimizers_builder.rs:54`, `lib/shard/src/optimize.rs:563`).

Memory placement overrides legacy flags, cached mmap pages remain evictable.
The `InRamChunkedMmap` enum comment describes locked RAM, but its constructor selects populated mmap and current placement semantics describe cached data, that comment should not be read as an unconditional OS pinning guarantee (`lib/segment/src/types.rs:2031`, `lib/segment/src/segment_constructor/segment_constructor_base/vector_storage.rs:139`, `lib/segment/src/types.rs:1840`).

# 4. Edge cases, failure modes and known sharp edges

1. **Replay correctness includes predicates, versions and partial batches.** Equal-version replay remains possible, failed transient operations are retained in the holder and retried from the first failure, while filter mutations must preserve their original target IDs, replaying a predicate against newer indexes can silently change its meaning (`lib/segment/src/segment/segment_ops.rs:621`, `lib/collection/src/collection_manager/collection_updater.rs:17`, `lib/collection/src/update_workers/optimization_worker.rs:523`, `lib/collection/src/shards/local_shard/resolve_submit.rs:1`).

2. **Deletion is logical before reclamation.** Ordinary point deletion clears payload state and drops the ID mapping, but propagation to vector deletion is deliberately disabled because historical cross-segment flush ordering could lose vector data, stale vector bytes can remain until rebuilding, and merge must let the highest-version tombstone defeat older live copies (`lib/segment/src/segment/segment_ops.rs:661`, `lib/segment/src/id_tracker/mod.rs:47`).
Append-only mode retains payload rows and field postings as well, filtering them through ID deletion state (`lib/segment/src/segment/segment_ops.rs:667`).

3. **Optimization needs spare disk and can increase retained history.** Admission estimates twice the source directories’ occupied size, unknown estimates permit an attempt, old sources survive until post-flush cleanup, and WAL pins can keep log files growing during long snapshots or replacement work (`lib/shard/src/optimize.rs:563`, `lib/shard/src/optimize.rs:478`, `lib/collection/src/update_workers/flush_workers.rs:21`).
The WAL disk watcher reserves headroom equal to twice configured WAL capacity, a full disk can therefore block ingestion before the next segment allocation (`lib/collection/src/shards/local_shard/mod.rs:275`, `lib/collection/src/shards/local_shard/shard_ops.rs:85`).

4. **Multi-file consistency requires several distinct repair mechanisms.** Mutable tracker histories tolerate incomplete tails, Gridstore journals repair torn pointers, and gap summaries can be rebuilt after inconsistent shutdown state, Gridstore still returns an error for inconsistent page counts and can panic if allocation remains impossible after rebuilding gaps (`lib/segment/src/id_tracker/mutable_id_tracker/mod.rs:92`, `lib/segment/src/id_tracker/mutable_id_tracker/versions_storage.rs:37`, `lib/blobstore/src/blobstore/gridstore/mod.rs:146`, `lib/blobstore/src/blobstore/gridstore/mod.rs:214`).
One questionable path deserves audit: clock-map persistence failure is reported but execution continues toward WAL acknowledgement, despite the adjacent requirement that clocks be durable first, its actual recovery consequence was not tested here (`lib/collection/src/update_workers/flush_workers.rs:119`).

5. **Deferred visibility creates another state machine.** An accepted update can be persisted but hidden until optimization, an older active head can remain visible beside its deferred replacement, and disabling or starving optimization can prolong that state **(inferred)** (`lib/collection/src/optimizers_builder.rs:104`, `lib/segment/src/id_tracker/point_mappings.rs:494`).
The contention documentation warns that a waiting deferred request stalls later updates, but this tree detaches the visibility wait so the queue keeps draining, source takes precedence (`documentation/ops-optimization/read-write-contention.md:29`, `lib/collection/src/update_workers/update_worker.rs:202`).

# 5. Features that are Cloud-only, enterprise-only or absent from the open-source build

The inspected core storage, WAL, optimizer and snapshot implementations are present in Apache-2.0 crates, the relevant manifests expose testing, staging or hardware features rather than an enterprise storage gate.
No commercial gate was found for the mechanisms covered here (`lib/segment/Cargo.toml:8`, `lib/segment/Cargo.toml:14`, `lib/wal/Cargo.toml:9`, `lib/blobstore/Cargo.toml:5`).

Snapshot storage supports local filesystem, S3, GCS and Azure in this source, these backends should not be classified as Cloud-only simply because they target cloud services (`lib/collection/src/common/snapshots_manager.rs:26`).

Append-only Logstore and serverless-compatible engine flags are also present, though their presence does not establish that Qdrant Cloud’s complete serverless control plane is included **(inferred)** (`lib/blobstore/src/config.rs:109`, `lib/common/common/src/flags.rs:95`).

Managed Cloud Backups are documented as physical disk-level copies, separate from collection snapshots.
Cloud also restricts URL restoration and startup CLI recovery operationally, those restrictions are deployment policies rather than missing snapshot primitives (`documentation/snapshots.md:18`, `documentation/snapshots.md:77`, `documentation/snapshots.md:94`).

`lib/wal` and `lib/blobstore` have `publish=false`, they are internal crates in this tree.
Their architecture is available for study, but this is not evidence of standalone published packages suitable for direct dependency adoption (`lib/wal/Cargo.toml:11`, `lib/blobstore/Cargo.toml:7`).

# 6. Design implications for nocturnum

The following are design proposals **(inferred)** from the cited mechanisms, rather than claims about existing nocturnum behavior.

## Copy conceptually

Adopt compact segment-local offsets, monotonic operation numbers and explicit durable boundaries.
Keep per-point versions separate from segment persistence progress, because one measures logical conflict resolution and the other controls replay retention (`lib/segment/src/segment/mod.rs:68`, `lib/segment/src/id_tracker/mod.rs:47`).

Make replacement publication a protocol with enforceable states: build privately, persist contents, publish readiness and membership, retain old recovery material, then reclaim it.
The strongest Qdrant lessons are the destination-before-source dependency and delayed destruction of replaced segments, both prevent data loss that ordinary idempotent WAL replay cannot repair (`lib/shard/src/segment_holder/flush.rs:75`, `lib/shard/src/optimize.rs:438`).

Resolve predicate mutations within the ordered writer before logging stable targets, or design an equivalent replay-stable representation.
Preserving only a predicate is insufficient when replay observes partially advanced payload/index state (`lib/collection/src/shards/local_shard/resolve_submit.rs:1`).

Use type-state where ordering mistakes are dangerous, `UnsyncedProxySegment::finalize` and segment readiness tokens demonstrate how constructors can make forgotten publication steps harder to express (`lib/shard/src/proxy_segment/mod.rs:42`, `lib/shard/src/segment_holder/mod.rs:150`).

## Minimal v1 cut

1. **One local writer stream per collection**, one dense float32 vector family, numeric/UUID external IDs, JSON payloads and explicit accepted/applied/durable acknowledgements, reducing the independent streams visible in Qdrant’s shard dispatch (`lib/collection/src/collection/point_ops.rs:161`).

2. **One durable WAL format with explicit framing and checksums**, initially ordinary buffered writes plus group commit, specify exactly which acknowledgement follows fsync, avoiding the source/docs ambiguity around mapped append (`lib/wal/src/segment.rs:48`, `lib/collection/src/update_workers/update_worker.rs:358`).

3. **Append-only record storage and immutable search segments**, with a RAM ID map, logical tombstones and one background rebuild worker, rebuilding naturally compacts mapping history and deleted records (`lib/segment/src/id_tracker/mutable_id_tracker/mod.rs:39`, `lib/shard/src/optimizers/vacuum_optimizer.rs:17`).

4. **A durable manifest controlling active generations**, publish through atomic replacement with deliberate file and directory synchronization, recovery should distinguish committed generations from abandoned builds before reclamation (`lib/segment/src/segment_constructor/segment_constructor_base/segment.rs:50`, `lib/common/common/src/fs/safe_delete.rs:55`).

5. **Full snapshots with a short writer fence**, archive immutable generations plus a bounded WAL interval, postpone partial manifests, nested proxies, serverless stores and deferred visibility until their complexity serves a measured requirement (`lib/collection/src/shards/local_shard/snapshot.rs:124`, `lib/shard/src/snapshots/snapshot_utils.rs:69`).

For payloads, start with an append-only blob log and offset table, reclaim through segment rebuilding.
Gridstore’s block reuse is useful later, but introduces allocation masks, gap repair, pointer journals and carefully ordered frees before nocturnum has established its basic crash contract (`lib/blobstore/src/blobstore/gridstore/mod.rs:33`, `lib/blobstore/src/blobstore/gridstore/mod.rs:608`).

Admission should account for serialized bytes as well as operation count, reserving space for WAL growth and at least one replacement generation.
Qdrant’s bounded queue and optimizer disk estimate provide the conceptual starting point, while neither alone bounds one huge mutation (`lib/collection/src/shards/local_shard/shard_ops.rs:121`, `lib/shard/src/optimize.rs:563`).

## Later phases and candidate Rust crates

Add multiple local shards, named vectors, sparse vectors and multivectors after recovery tests cover overlapping generations, deletes and failed publication.
Add cached/cold mmap placement before io_uring, then consider compressed/disk ID maps, block reuse, partial snapshots and object storage as independent later phases (`lib/segment/src/vector_storage/vector_storage_base.rs:622`, `lib/segment/src/common/io_uring.rs:88`, `lib/segment/src/id_tracker/disk_id_tracker/on_disk_format.rs:1`).

Candidate dependencies already used in the inspected tree are:

| Purpose | Candidates |
|---|---|
| Async scheduling and bounded queues | `tokio`, `tokio-util`, with blocking storage work outside executor threads |
| Synchronization | `parking_lot`, `arc-swap`, standard atomics |
| Binary formats and IDs | `serde`, `serde_json`, `byteorder`, `uuid`, carefully validated `bytemuck` layouts |
| Files and integrity | `memmap2`, `fs4`, `rustix`, `crc32c`, `sha2`, `tempfile` |
| Later storage features | `lz4_flex`, `roaring`, `object_store`, Linux `io-uring` |

These are candidates for evaluation rather than a complete dependency prescription, their presence is evidenced by `lib/segment/Cargo.toml:49`, `lib/wal/Cargo.toml:16`, `lib/blobstore/Cargo.toml:12`, `lib/collection/Cargo.toml:57` and `lib/common/common/Cargo.toml:69`.

Use model-based storage tests and process-crash tests as early acceptance gates.
Qdrant’s Gridstore article describes comparing operations with a simple map and testing failures around persistence, nocturnum should additionally kill the writer at each manifest-publication and reclamation boundary proposed above (`articles/gridstore-key-value-storage.md:117`, `articles/gridstore-key-value-storage.md:164`).

# 7. Open questions

1. **Choose nocturnum’s acknowledgement contract first:** should the default response mean accepted, applied or durable, and should visibility be independently selectable?
Qdrant distinguishes WAL, segment and visible waits, while its docs overstate unsynced append durability (`lib/collection/src/shards/shard_trait.rs:23`, `documentation/manage-data/storage.md:95`).

2. **What is the required batch atomicity?** Per-point versions support replay of partially completed work, but they do not establish whole-batch or cross-shard atomicity.
The minimal manifest/WAL protocol needs an explicit answer before its record format is fixed (`lib/segment/src/segment/segment_ops.rs:621`, `lib/collection/src/collection/point_ops.rs:161`).

3. **Should updates remain immediately visible during indexing backlog?** Deferred visibility protects query latency but adds active/deferred heads and waiting semantics.
Its value should be demonstrated before nocturnum adopts that state machine (`lib/segment/src/id_tracker/point_mappings.rs:27`, `lib/collection/src/update_workers/update_worker.rs:202`).

4. **Which durability paths need additional verification?** In this tree, clock persistence failure can precede acknowledgement, rotated WAL segments flush asynchronously while `SerdeWal::flush` targets the open segment, and snapshot checksum publication uses buffered flush.
These observations require fault-injection analysis before deriving stronger guarantees (`lib/collection/src/update_workers/flush_workers.rs:119`, `lib/wal/src/lib.rs:264`, `lib/shard/src/wal.rs:254`, `lib/collection/src/common/snapshots_manager.rs:485`).

5. **What workload justifies additional storage modes?** Required point counts, payload sizes, mutation rate and memory budget should decide when nocturnum needs disk mappings, multivectors, reusable blob blocks or object storage, each adds a separate persistence and recovery surface (`lib/segment/src/id_tracker/disk_id_tracker/mod.rs:39`, `lib/segment/src/vector_storage/multi_dense/appendable_mmap_multi_dense_vector_storage.rs:42`, `lib/blobstore/src/blobstore/mod.rs:34`).