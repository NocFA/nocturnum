# 1. Summary

Scope: the supplied engine checkout identifies itself as v1.19.2, engine citations are relative to the root of the `qdrant/qdrant` repository, documentation citations are relative to `qdrant-landing/content` in the `qdrant/landing_page` repository, and UI citations start with `qdrant-web-ui/`, the root of the `qdrant/qdrant-web-ui` repository.
This is static research, no builds, runtime tests or file writes were performed. (`Cargo.toml:3`)

- **Qdrant separates metadata consensus from point replication**, one cluster-wide Raft group agrees on membership, collection configuration, aliases, shard placement and lifecycle operations, while point writes travel directly between shard replicas, so replication does not imply atomic distributed transactions. (`lib/storage/src/content_manager/mod.rs:50`; `lib/storage/src/dispatcher.rs:80`; `lib/collection/src/shards/replica_set/update.rs:145`; `documentation/scaling/horizontal-scaling.md:79`)
- **Sharding and replication are independent dimensions**, automatic sharding hashes point IDs into logical shards, custom sharding selects a shard-key-specific hash ring, and each logical shard has a `ShardReplicaSet` spanning peers, with replication and write-consistency defaults both equal to one. (`lib/collection/src/hash_ring.rs:18`; `lib/collection/src/shards/shard_holder/mod.rs:79`; `lib/collection/src/shards/replica_set/mod.rs:100`; `lib/collection/src/config.rs:355`)
- **Consistency settings have narrower meanings than their names suggest**, medium and strong write ordering serialize through a replica-set coordinator selected by peer ID, read consistency compares returned records, and failed writes can already have modified replicas or other shards, with no rollback in the inspected path. (`lib/collection/src/shards/replica_set/update.rs:169`; `lib/collection/src/shards/replica_set/update.rs:218`; `lib/collection/src/shards/resolve.rs:169`; `lib/collection/src/collection/point_ops.rs:234`)
- **The supplied docs contain material discrepancies**, current source defaults ordinary transfers to snapshots, although deployment docs still label record streaming the default, and the write-consistency threshold appears bypassable when non-updatable replicas are skipped and every attempted write succeeds, the latter is an untested static inference. (`lib/collection/src/collection/shard_transfer.rs:75`; `documentation/scaling/distributed_deployment.md:289`; `lib/collection/src/shards/replica_set/update.rs:343`; `lib/collection/src/shards/replica_set/update.rs:541`)
- **Tenant partitioning and promotion exist in the open engine**, `is_tenant` improves physical layout and indexing, fallback routing plus filtered transfers supports promotion to dedicated shards, while automated balancing, replication-factor reconciliation, node replacement and supported resharding orchestration belong to Cloud operations, the resharding state transitions themselves are present in source. (`lib/shard/src/optimize.rs:226`; `lib/collection/src/shards/shard_holder/mod.rs:824`; `src/common/collections.rs:382`; `src/common/collections.rs:684`; `documentation/scaling/distributed_deployment.md:382`; `documentation/scaling/resilience.md:60`)

# 2. How it works, fundamentally

## Topology and transport

Every peer has a persistent `PeerId`, an address book and collection metadata, any node receiving a public request can coordinate its execution against local or remote shards.
The default endpoints are REST on 6333, public gRPC on 6334 and internal peer gRPC on 6335, with distributed mode disabled initially. (`lib/storage/src/content_manager/consensus/persistent.rs:31`; `lib/collection/src/shards/channel_service.rs`; `config/config.yaml:370`; `config/config.yaml:385`; `config/config.yaml:461`)

The internal listener hosts Raft and internal data services through Tonic, its bind address can differ from the public service address, and peer TLS is configurable.
Internal API authentication is explicitly opt-in in this checkout, `enforce_internal_auth` accepts write-capable API keys and rejects read-only keys and JWTs, so public API authentication alone does not establish an authenticated internal boundary. (`src/tonic/mod.rs:240`; `src/tonic/mod.rs:263`; `src/settings.rs:140`; `config/config.yaml:431`; `config/config.yaml:476`)

Bootstrap uses another peer’s internal address, a joining node enters as a learner before becoming a voter.
The leader proposes learner promotion only when its log has no uncommitted tail and a learner’s replicated index matches the commit index, giving membership changes a catch-up stage before adding voting responsibility. (`src/consensus.rs:561`; `src/consensus.rs:955`; `src/consensus.rs:992`)

Peer identity persists independently of its current address, restart recovery can announce a changed URI through another known peer.
Fresh generated IDs are restricted to the exact-integer range used by JSON consumers, rather than using arbitrary full-width random integers. (`src/consensus.rs:498`; `lib/storage/src/content_manager/consensus/persistent.rs:352`)

## Raft implementation and authoritative state machine

The consensus implementation uses TiKV’s **`raft-rs`**, pinned to revision `aafb07c7bab439c6139926a77dfafc5b10e9bc84`, with `prost-codec`, the workspace also pins the Prost version used by Raft to 0.11.9. (`Cargo.toml:267`)

Important types are `Consensus`, `RawNode<ConsensusStateRef>`, `ConsensusManager<C>`, `Persistent`, `ConsensusOpWal`, `TableOfContent`, `CollectionContainer` and `ConsensusOperations`.
`Consensus` drives Raft in dedicated threads, while shared persistent state and its WAL use `parking_lot` locks, asynchronous application and networking use Tokio runtimes and channels. (`src/consensus.rs:40`; `src/consensus.rs:62`; `src/consensus.rs:109`; `src/consensus.rs:131`; `lib/storage/src/content_manager/consensus_manager.rs:85`)

The committed operation vocabulary covers collection creation, updates and deletion, aliases, shard-key creation and deletion, transfers, resharding, replica-state changes, named-vector changes and payload-index metadata.
Peer and cluster metadata and cluster quota configuration also pass through consensus, while adding or removing peers becomes Raft `ConfChangeV2` entries, and snapshot-request/report variants are internal signals handled directly by the consensus thread. (`lib/storage/src/content_manager/consensus_state_machine/mod.rs:128`; `lib/storage/src/content_manager/mod.rs:50`; `lib/storage/src/content_manager/consensus_manager.rs:628`)

**Point upserts, payload mutations and deletes do not enter this Raft log**, their coordinator splits the request by shard and invokes each replica set’s update path.
A successful metadata operation therefore establishes a different guarantee from a successful point operation. (`lib/collection/src/collection/point_ops.rs:174`; `lib/collection/src/shards/replica_set/update.rs:145`; `documentation/scaling/horizontal-scaling.md:81`)

The authoritative application path is currently `ConsensusManager::apply_consensus_op` into `TableOfContent::perform_collection_meta_op`.
The separately named `ConsensusStateMachine` is an optional shadow validator, disabled by default, it plans ordered `Action`s against `ClusterState` and compares its expected result with the existing handlers, so its presence must not be read as a completed replacement of those handlers. (`lib/storage/src/content_manager/consensus_manager.rs:593`; `lib/storage/src/content_manager/consensus_manager.rs:628`; `lib/storage/src/content_manager/consensus_shadow/mod.rs:23`)

That planner documents a useful replay contract, emit only missing idempotent actions, never reject a partially applied operation, and write its completion marker last.
Its `NodeContext` still contains node-local defaults that may differ between peers, while collection placement and UUIDs are resolved before proposing collection creation, reducing nondeterministic work during application. (`lib/storage/src/content_manager/consensus_state_machine/mod.rs:10`; `lib/storage/src/content_manager/consensus_state_machine/mod.rs:202`; `lib/storage/src/dispatcher.rs:87`)

Application distinguishes user errors from service failures, a user error is considered an applied but rejected command, whereas a service error stops consensus application.
The applied-entry queue is persisted after each successful application step, allowing restart replay of committed work that did not finish locally. (`lib/storage/src/content_manager/consensus_manager.rs:391`; `lib/storage/src/content_manager/consensus_manager.rs:435`; `lib/storage/src/content_manager/consensus_manager.rs:476`)

## Consensus persistence and snapshots

`Persistent` stores Raft hard state, configuration state, latest snapshot metadata, unapplied-entry progress, peer addresses, peer metadata, cluster metadata and this node’s identity in **`raft_state.json`**, saved with an atomic-file replacement.
Collection data and metadata live in their own regular storage rather than being embedded in that file. (`lib/storage/src/content_manager/consensus/persistent.rs:26`; `lib/storage/src/content_manager/consensus/persistent.rs:31`; `lib/storage/src/content_manager/consensus/persistent.rs:398`)

`ConsensusOpWal` stores protobuf-encoded Raft entries under **`collections_meta_wal/`**, normal command payloads inside those entries are CBOR-encoded `ConsensusOperations`.
Conflicting uncommitted entries can truncate the physical WAL, and appending consensus entries explicitly flushes its open segment before returning. (`lib/storage/src/content_manager/consensus/consensus_wal.rs:14`; `lib/storage/src/content_manager/mod.rs:74`; `lib/storage/src/content_manager/consensus/consensus_wal.rs:206`; `lib/storage/src/content_manager/consensus/consensus_wal.rs:229`; `lib/storage/src/content_manager/consensus/consensus_wal.rs:258`)

The underlying WAL uses fixed-length mmap-backed segments, an eight-byte segment header contains `wal`, format version zero and a CRC seed, entries contain a little-endian length, data, alignment padding and CRC32-C.
This format supplies corruption detection and append recovery mechanics, its encoding is separate from the protobuf or CBOR payload placed inside each entry. (`lib/wal/src/segment.rs:18`; `lib/wal/src/segment.rs:46`)

A Raft snapshot is a CBOR-encoded `SnapshotData`, containing collection states and aliases, topology, peer metadata, cluster metadata and quota configuration.
It is a **metadata snapshot**, distinct from a shard snapshot containing vectors and indexes, its metadata uses the commit index and the term of the entry at that index. (`lib/storage/src/content_manager/consensus_manager.rs:57`; `lib/storage/src/content_manager/consensus_manager.rs:1359`)

Snapshot recovery applies collection metadata, persists replacement Raft state and only then clears the obsolete consensus WAL.
Startup repairs the crash window where new snapshot state was saved but the old WAL remained, while `/cluster/recover` requests a consensus snapshot rather than reconstructing missing vectors. (`lib/storage/src/content_manager/consensus_manager.rs:127`; `lib/storage/src/content_manager/consensus_manager.rs:711`; `src/actix/api/cluster_api.rs:60`)

## Shard placement, routing and replica lifecycle

`ShardHolder` owns a deterministic `BTreeMap<ShardId, Arc<ShardReplicaSet>>`, persistent transfers, persistent resharding state, shard-key mappings and hash rings.
Automatic collections use one ring under `None`, custom collections use a ring for each named shard key, each fair ring has 100 virtual positions per logical shard and uses stable hashing with SipHash. (`lib/collection/src/shards/shard_holder/mod.rs:68`; `lib/collection/src/hash_ring.rs:15`; `lib/collection/src/hash_ring.rs:187`; `lib/collection/src/hash_ring.rs:228`)

Collection creation proposes placement using a min-heap of per-collection shard counts with randomized tie-breaking, the replica count is capped at the number of available peers.
This is initial count-based placement, it contains no failure-domain or shard-size input, and custom collections start without shards until shard keys are created. (`lib/storage/src/content_manager/shard_distribution.rs:12`; `lib/storage/src/content_manager/shard_distribution.rs:74`; `lib/storage/src/dispatcher.rs:119`)

With automatic sharding, `shard_number` is the collection’s logical shard count, absent an override it defaults to the peer count at creation.
With custom sharding it supplies the default number of logical shards per shard key, so ten keys, one shard per key and replication factor two imply twenty physical replicas, creating many keys therefore creates substantial independent shard overhead. (`lib/storage/src/content_manager/toc/mod.rs:550`; `documentation/scaling/distributed_deployment.md:227`)

`ShardReplicaSet` contains a local `Shard` variant, remote handles, persisted `ReplicaSetState`, local failure suppression, a write-ordering mutex and a clock set.
The `Shard` abstraction permits temporary forwarding and queue proxies during transfer, while `replica_state.json`, `shard_key_mapping.json`, `shard_transfers` and `resharding_state.json` preserve routing and lifecycle metadata. (`lib/collection/src/shards/replica_set/mod.rs:100`; `lib/collection/src/shards/replica_set/mod.rs:140`; `lib/collection/src/shards/shard_holder/mod.rs:68`)

| Replica states | Operational meaning |
|---|---|
| `Active`, `Dead` | Active is readable and updatable, Dead receives neither ordinary reads nor ordinary writes and is eligible for automatic recovery. |
| `Partial`, `Initializing`, `Listener` | Partial receives migration data, Initializing belongs to creation, Listener receives updates without serving reads, all three accept ordinary updates. |
| `Recovery`, deprecated `PartialSnapshot`, `ManualRecovery` | Ordinary reads and writes are disabled, internal forced writes have state-specific exceptions, ManualRecovery survives startup without being treated as an automatically recoverable failure. |
| `Resharding`, `ReshardingScaleDown` | Scale-up migration state receives writes without ordinary reads, scale-down state remains readable and active during its migration role. |
| `ActiveRead` | Readable transitional state that accepts updates but is not classified as fully Active, used to switch readers before completing the writer handover. |

The distinctions come from separate predicates for readability, updatability, health, active status and ability to serve as a source of truth, so a single healthy/unhealthy boolean would lose essential behavior. (`lib/collection/src/shards/replica_set/replica_set_state.rs:100`; `lib/collection/src/shards/replica_set/replica_set_state.rs:135`; `lib/collection/src/shards/replica_set/update.rs:67`)

## Writes, ordering and read consistency

The receiving coordinator sends a point operation to all currently updatable, locally enabled replicas, concurrently subject to `update_concurrency`.
It adds a `ClockTag` identifying coordinator peer, clock ID and tick, receivers reject stale ticks, echo newer clock values and allow the coordinator up to three clock-rejection attempts, this provides replay/order bookkeeping without creating a global point transaction log. (`lib/collection/src/shards/replica_set/update.rs:24`; `lib/collection/src/shards/replica_set/update.rs:343`; `lib/collection/src/shards/replica_set/update.rs:382`; `lib/collection/src/shards/local_shard/clock_map.rs:59`)

Write ordering selects the coordinator per replica set:

| Ordering | Coordinator and serialization |
|---|---|
| `weak`, default | The receiving peer coordinates, without the ordering mutex. |
| `medium` | Highest-ID eligible source-of-truth replica, serialized by its ordering mutex. |
| `strong` | Highest-ID replica in the membership map regardless of current eligibility, serialized if reachable, unavailable when that selected peer cannot serve. |

These coordinators are separate from the metadata Raft leader, “dynamically elected” in the docs describes deterministic eligibility-based selection rather than a second Raft election.
The horizontal-scaling page also describes leaders as dynamically selected even when discussing strong ordering, the implementation’s maximum-member rule is the authoritative distinction. (`lib/collection/src/shards/replica_set/update.rs:169`; `lib/collection/src/shards/replica_set/update.rs:218`; `documentation/scaling/consistency-guarantees.md:109`; `documentation/scaling/horizontal-scaling.md:71`)

`write_consistency_factor` computes a minimum successful-response count, capped at the replica handles currently present.
When attempted writes fail but enough succeed, the coordinator suppresses failed replicas locally and proposes state changes through consensus, callback-based waits can additionally wait up to thirty seconds for their deactivation, whereas insufficient successes return an error after whatever writes have already happened. (`lib/collection/src/shards/replica_set/update.rs:481`; `lib/collection/src/shards/replica_set/update.rs:559`; `lib/collection/src/shards/replica_set/update.rs:579`; `lib/collection/src/shards/replica_set/update.rs:620`)

A successful response must include an eligible source-of-truth replica, listener-only success is insufficient.
Listener writes use WAL acknowledgement, ordinary `wait=false` maps to `WaitUntil::Wal`, and `wait=true` maps to visibility, with an intermediate segment-applied wait also represented internally. (`lib/collection/src/shards/replica_set/update.rs:104`; `lib/collection/src/shards/replica_set/update.rs:637`; `lib/collection/src/shards/shard_trait.rs:23`)

Read consistency operates **within each selected logical shard**, using total configured replicas rather than total cluster peers:

| Consistency | Required responses and reconciliation |
|---|---|
| `1`, default | One successful replica response. |
| Integer `N` | `N` clamped to the replica count, retain matching results across those responses. |
| `quorum` | `floor(R/2)+1` successful responses, retain agreement across all responses collected. |
| `majority` | Request responses from all replicas, retain results agreeing in a majority. |
| `all` | Request responses from all replicas, retain agreement across all. |

Initializing replicas receive a special adjustment to the required response count.
The implementation agrees with the public consistency guide, but the introductory comment in `consistency_params.rs` reverses majority and quorum descriptions, callers should follow executable dispatch rather than that comment. (`lib/collection/src/shards/replica_set/execute_read_operation.rs:63`; `lib/collection/src/operations/consistency_params.rs:17`; `documentation/scaling/consistency-guarantees.md:72`)

Record reconciliation compares ID, ordering value, requested vector and payload, scored-point reconciliation also compares the score.
It filters conflicting returned versions instead of selecting the newest version or repairing storage, count reconciliation uses a minimum for `all` and a median for `majority`. (`lib/collection/src/shards/resolve.rs:35`; `lib/collection/src/shards/resolve.rs:136`; `lib/collection/src/shards/resolve.rs:169`)

## Search fan-out and result merging

The collection coordinator selects logical shards, launches their searches concurrently and asks each replica set to satisfy the requested read consistency.
Default replica selection prefers a readable local shard and randomizes remote order, it can hedge against local update activity or configured delay, failed read attempts fall back to remaining candidates without reporting those failures to consensus. (`lib/collection/src/collection/search.rs:167`; `lib/collection/src/shards/replica_set/execute_read_operation.rs:18`; `lib/collection/src/shards/replica_set/execute_read_operation.rs:249`; `lib/collection/src/shards/replica_set/execute_read_operation.rs:333`)

`X-Qdrant-Route-Affinity` produces a `RoutingToken`, candidates are ordered by a stable token-and-peer hash, removing local preference and default speculative fan-out.
Explicit fan-out settings, delayed hedging, failure and membership changes can still change the answering replica, so affinity supplies locality rather than a snapshot or session-consistency guarantee. (`lib/collection/src/shards/replica_set/execute_read_operation.rs:155`; `lib/collection/src/shards/replica_set/execute_read_operation.rs:275`; `documentation/scaling/consistency-guarantees.md:87`)

Core search merges sorted shard results according to score direction, deduplicates point IDs and applies the client offset once at the collection level.
Any selected shard error propagates through `try_join_all`, while universal queries have additional intermediate-result aggregation and probabilistic shard undersampling for sufficiently large non-exact queries. (`lib/collection/src/collection/search.rs:206`; `lib/collection/src/collection/search.rs:285`; `lib/collection/src/collection/query.rs:71`; `lib/collection/src/collection/query.rs:330`)

## Transfers and online resharding

`ShardTransfer` identifies source and destination peers, source shard, optional destination shard, method, optional filter and `sync`.
A synchronized transfer retains the source replica, a move removes it only after destination activation, start, restart, state-switch, finish and abort operations use consensus, while bulk contents travel outside consensus. (`lib/collection/src/shards/transfer/mod.rs:102`; `lib/storage/src/content_manager/toc/transfer.rs:69`; `lib/collection/src/collection/shard_transfer.rs:448`)

**`stream_records`** installs a forward proxy, drains pre-existing update work, transfers payload-index definitions and scrolls records in batches of 100 while forwarding concurrent updates.
The prepared batch retains a proxy update lock across reading and sending, the outer shard-holder lock is released before network transmission, vector indexes and quantization are rebuilt on the destination. (`lib/collection/src/shards/transfer/stream_records.rs:18`; `lib/collection/src/shards/transfer/stream_records.rs:78`; `lib/collection/src/shards/transfer/stream_records.rs:136`; `documentation/scaling/distributed_deployment.md:300`)

**`snapshot`** installs a queue proxy, recovers a source snapshot on a non-updatable destination, switches it to Partial, then replays queued operations and converts to forwarding.
Modern compatible peers use an HTTP streaming snapshot endpoint, older peers use a materialized snapshot file, the transfer consequently requires working REST connectivity as well as internal gRPC, and carries indexes and quantization. (`lib/collection/src/shards/transfer/snapshot.rs:185`; `lib/collection/src/shards/transfer/snapshot.rs:196`; `lib/collection/src/shards/transfer/snapshot.rs:240`; `documentation/scaling/distributed_deployment.md:299`)

**`wal_delta`** preserves an existing destination in Recovery, obtains its recovery clocks, finds a covering suffix of the source WAL, queue-proxies from that boundary and replays missed operations.
`RecoverableWal` tracks newest clocks and recovery cutoffs, a deactivation-time clock snapshot prevents a failed intermediate transfer from falsely advancing the next recovery boundary, an empty resolved delta still captures newly arriving writes. (`lib/collection/src/shards/transfer/wal_delta.rs:86`; `lib/collection/src/shards/transfer/wal_delta.rs:105`; `lib/collection/src/wal_delta.rs:14`; `lib/collection/src/wal_delta.rs:74`)

Delta resolution rejects empty recovery clocks, unknown or ahead-of-source clocks, truncated history, untagged records and unreadable WAL entries.
Failure restarts through consensus using a full-transfer fallback, currently normally snapshot, and the driver waits for metadata synchronization before dismantling forwarding, preventing lagging peers from omitting writes to the destination. (`lib/collection/src/wal_delta.rs:208`; `lib/collection/src/wal_delta.rs:285`; `lib/collection/src/shards/transfer/driver.rs:119`; `lib/collection/src/shards/transfer/driver.rs:137`)

Resharding changes one shard at a time, `HashRingRouter::Resharding` holds old and new rings and routes writes to their deduplicated union.
Scale-up creates a new Resharding replica, scale-down migrates the removed shard into surviving shards, filtered streaming selects points by destination ownership, then metadata advances through `MigratingPoints`, `ReadHashRingCommitted` and `WriteHashRingCommitted` before final cleanup. (`lib/collection/src/hash_ring.rs:57`; `lib/collection/src/hash_ring.rs:140`; `lib/collection/src/shards/resharding.rs:75`; `lib/collection/src/shards/transfer/resharding_stream_records.rs:57`; `lib/collection/src/collection/resharding.rs:93`)

Reader routing excludes an incomplete scale-up shard and excludes a removed scale-down shard after write-ring commitment, finishing scale-down persists the reduced count before deleting its directory.
Only one resharding operation may run per collection, and removal of the last shard is rejected. (`lib/collection/src/shards/shard_holder/mod.rs:773`; `lib/collection/src/collection/resharding.rs:249`; `src/common/collections.rs:773`; `lib/collection/src/shards/shard_holder/resharding.rs:92`)

## Multi-tenancy and promotion

Payload partitioning puts tenants in a shared collection with a tenant-identifying payload field and a mandatory application filter.
`is_tenant=true` on keyword or UUID indexes influences optimizer defragmentation and tenant-specific graph construction, points are grouped during segment rebuilding, so the docs’ sequential-read description is a layout optimization rather than a guarantee that a tenant occupies one globally contiguous extent. (`documentation/manage-data/multitenancy.md:27`; `lib/segment/src/types.rs:2714`; `lib/shard/src/optimize.rs:226`; `lib/segment/src/segment_constructor/segment_builder.rs:290`; `lib/segment/src/index/hnsw_index/hnsw/build/additional_links.rs:145`)

For tenant-filtered workloads, the docs recommend `m=0` and a nonzero `payload_m` to build payload-group graph links without a global graph, at the cost of slower cross-tenant searches.
Collection-per-tenant carries independent collection overhead, its documented use is a limited tenant count needing stronger isolation. (`documentation/manage-data/multitenancy.md:15`; `documentation/manage-data/multitenancy.md:62`)

Tiered tenancy combines custom shard keys with `{target, fallback}` routing.
Reads choose the dedicated target only when every target shard has a readable replica, otherwise they use the fallback, writes use fallback while the target is Partial, duplicate into both during the ActiveRead handover, and use the target once active. (`lib/collection/src/shards/shard_holder/mod.rs:824`; `lib/collection/src/shards/shard_holder/mod.rs:868`; `lib/collection/src/shards/shard_holder/mod.rs:941`)

Promotion creates a Partial target and calls `replicate_points` with a tenant filter, the API enforces one source logical shard and one destination replica, filtered transfer selects record streaming.
It switches the destination to ActiveRead and synchronizes peers before completion, then activates it, source tenant data is retained until explicitly deleted after promotion. (`src/common/collections.rs:382`; `lib/collection/src/shards/transfer/stream_records.rs:178`; `documentation/manage-data/multitenancy.md:186`)

# 3. Config knobs and defaults that matter

| Configuration group | Defaults and significance |
|---|---|
| Cluster and transport | `cluster.enabled=false`, peer port 6335 from base YAML, peer TLS false, connection pool size two, connect timeout two seconds and ordinary gRPC timeout sixty seconds, the optional P2P bind host falls back to the service host. (`config/config.yaml:461`; `src/settings.rs:140`; `lib/api/src/grpc/transport_channel_pool.rs:25`) |
| Consensus | Tick 100 ms, incoming message queue 100, bootstrap timeout fifteen seconds, message timeout ten ticks and compaction threshold 128 applied entries, zero disables compaction. (`src/settings.rs:573`; `config/config.yaml:488`) |
| Collection distribution | Auto sharding, one shard on standalone or generally one per peer at distributed creation, replication factor one, write-consistency factor one, collection defaults and explicit request values override the initial distribution. (`lib/collection/src/config.rs:81`; `lib/collection/src/config.rs:355`; `lib/storage/src/content_manager/toc/mod.rs:550`) |
| Request consistency and scheduling | Weak write ordering, read consistency one, `wait=false` acknowledges WAL submission, read fan-out auto-selects zero extras for a ready local replica and one otherwise, delay zero disables delayed hedging, update concurrency null allows maximum concurrency. (`lib/collection/src/shards/replica_set/update.rs:218`; `lib/collection/src/operations/consistency_params.rs:54`; `lib/collection/src/shards/shard_trait.rs:57`; `lib/collection/src/shards/replica_set/execute_read_operation.rs:275`; `config/config.yaml:102`) |
| Recovery and migration | Ordinary transfers default to Snapshot in current source, automatic dead-replica recovery prefers WalDelta when all peers support it, automatic incoming/outgoing limits default to one per collection per node, point WAL segments default to 32 MB with one retained closed segment, resharding is disabled. (`lib/collection/src/collection/shard_transfer.rs:75`; `lib/collection/src/collection/mod.rs:866`; `config/config.yaml:106`; `config/config.yaml:140`; `src/settings.rs:136`) |

Automatic transfer limits exclude user-requested transfers, so manual operations need their own capacity planning.
Changing `replication_factor` on an existing self-hosted collection changes configuration without automatically reconciling placement, whereas Cloud adjusts actual replicas, configuring a higher number alone does not create resilience. (`config/config.yaml:140`; `documentation/scaling/distributed_deployment.md:382`)

# 4. Edge cases, failure modes and known sharp edges

## Partition and loss semantics

Raft prevents independently committed metadata histories through majority agreement, point operations have no equivalent cluster-wide quorum gate.
During partition, reads may continue against reachable replicas, weak writes may execute on either side, while writes needing failed-replica deactivation can time out waiting for consensus, this partition behavior is **(inferred)** from the separate operation paths and deactivation wait, no partition experiment was run. (`lib/storage/src/content_manager/consensus_manager.rs:949`; `lib/collection/src/shards/replica_set/update.rs:145`; `lib/collection/src/shards/replica_set/update.rs:579`)

A two-voter cluster loses metadata quorum when either voter disappears, even if the surviving machine holds every vector.
Strong ordering can independently lose write availability when its highest-ID shard member fails, three voters and two replicas solve different failure requirements, and blanket availability claims in the resilience docs need qualification for explicit ordering and consistency settings. (`documentation/scaling/horizontal-scaling.md:27`; `lib/collection/src/shards/replica_set/update.rs:218`; `documentation/scaling/resilience.md:39`)

Node removal normally refuses peers holding shards, forced removal bypasses that protection but still proposes a consensus membership change, it does not replace missing quorum.
Permanent replacement requires attaching a new peer and arranging replicas, returning Dead replicas automatically request recovery from healthy sources, quota pressure or transfer limits can postpone it, all-copy loss requires a data snapshot. (`src/actix/api/cluster_api.rs:75`; `lib/collection/src/collection/mod.rs:832`; `lib/collection/src/collection/mod.rs:883`; `documentation/scaling/node-failure-recovery.md:20`)

Last-source protections refuse ordinary deactivation of the final active replica, with additional protection for lone Initializing or Listener copies.
A physically failed final copy can therefore remain logically protected without becoming serviceable, a replica-state map is not proof of disk health. (`lib/collection/src/collection/mod.rs:484`; `lib/collection/src/shards/replica_set/mod.rs:449`)

## Partial writes and threshold enforcement

A collection request collects outcomes from all participating shard writes, returning `InconsistentShardFailure` when only a subset failed.
Within one replica set, insufficient acknowledgements likewise return after writes have already occurred, client retry must treat an error as an uncertain or partial result. (`lib/collection/src/collection/point_ops.rs:234`; `lib/collection/src/shards/replica_set/update.rs:620`; `documentation/scaling/consistency-guarantees.md:55`)

**Potential threshold bypass, (inferred):** remotes are filtered to updatable replicas, but `replica_count` counts all handles, and the successful-count comparison appears only inside `if !failures.is_empty()`.
With one Active replica, one already Dead replica and write-consistency factor two, a successful write to the Active replica appears able to pass without two responses, this contradicts the guide’s claim that insufficient active replicas always fail and needs a focused runtime test. (`lib/collection/src/shards/replica_set/update.rs:343`; `lib/collection/src/shards/replica_set/update.rs:489`; `lib/collection/src/shards/replica_set/update.rs:541`; `lib/collection/src/shards/replica_set/update.rs:637`; `documentation/scaling/consistency-guarantees.md:40`)

Point WAL acknowledgement also needs careful terminology, submission appends and dispatches without an explicit per-request flush, `Segment::append` states durability requires flushing, and a separate periodic worker flushes the WAL.
Thus WAL acceptance should not be interpreted as proven power-loss-safe persistence for that request **(inferred)**, despite stronger durability language in wrapper comments. (`lib/collection/src/shards/local_shard/shard_ops.rs:105`; `lib/shard/src/wal.rs:101`; `lib/wal/src/segment.rs:331`; `lib/collection/src/update_workers/flush_workers.rs:62`; `lib/collection/src/update_workers/flush_workers.rs:149`)

## Query, transfer and tenant hazards

Read reconciliation can discard a point whose requested payload or score differs between replicas, ANN candidate variation can therefore reduce returned results **(inferred)**, it does not prove linearizability.
The query undersampling implementation explicitly accepts some risk of retrieving too few candidates, exact requests bypass that optimization. (`lib/collection/src/shards/resolve.rs:176`; `lib/collection/src/collection/query.rs:71`)

Duplicate IDs across custom shard keys are stored independently, but collection-level merging deduplicates by point ID, so cross-key reads can collapse results.
A missing shard selector can span tenant keys, and `is_tenant` supplies optimization without access enforcement, this checkout’s old JWT payload constraints are explicitly rejected, leaving collection-level access modes and an application-enforced tenant boundary. (`documentation/scaling/distributed_deployment.md:256`; `lib/collection/src/collection/search.rs:318`; `lib/collection/src/shards/shard_holder/mod.rs:696`; `lib/storage/src/rbac/mod.rs:45`)

Transfer finalization depends on metadata propagation, disk capacity, compatible peer versions and connectivity appropriate to the method.
The driver has three attempts with increasing retry delay, snapshot and delta cancellation can leave partial destination state, and persisted transfer records plus local task status are reconciled during lifecycle synchronization. (`lib/collection/src/shards/transfer/driver.rs:23`; `lib/collection/src/shards/transfer/snapshot.rs:150`; `lib/collection/src/shards/transfer/wal_delta.rs:67`; `lib/collection/src/collection/mod.rs:754`)

Lock behavior varies by query path, core search clones shard `Arc`s and releases the holder before awaiting, while universal shard querying retains its holder read guard across fan-out.
Long universal requests can therefore delay holder writers **(inferred)**, lifecycle code also contains explicit protections against update-wait/transfer lock deadlocks and crash-replay ordering failures. (`lib/collection/src/collection/search.rs:167`; `lib/collection/src/collection/query.rs:159`; `lib/collection/src/shards/replica_set/update.rs:338`; `lib/collection/src/collection/resharding.rs:17`)

# 5. Features that are Cloud-only, enterprise-only or absent from the open-source build

Supported automated resharding is Cloud-only in the docs, including Hybrid and Private Cloud.
The open engine contains resharding operations, rings and transfer machinery, but `start_resharding` rejects the default-disabled setting with a Cloud-only message, and the automatic `drive_resharding` call is commented out, enabling the flag alone therefore does not establish complete automated orchestration **(inferred)**. (`documentation/cloud/cluster-scaling.md:48`; `src/common/collections.rs:684`; `src/settings.rs:136`; `lib/collection/src/collection/resharding.rs:140`)

Cloud supplies continuous count/size balancing, reconciliation to configured replication factor, automatic permanent-node replacement, cluster scaling and rolling-restart management.
Self-hosting exposes the underlying move, replicate and remove operations, while operators must supply the control loop and deployment tooling. (`documentation/cloud/configure-cluster.md:58`; `documentation/cloud/configure-cluster.md:66`; `documentation/scaling/distributed_deployment.md:382`; `documentation/scaling/resilience.md:60`; `documentation/cloud/cluster-scaling.md:34`)

Automatic zone-aware placement is documented for Cloud premium, self-hosted placement has no zone awareness.
The Hybrid operator configuration separately describes a Cluster Manager endpoint and backup management, providing evidence that these operational components sit outside the supplied engine’s basic replication path. (`documentation/scaling/resilience.md:48`; `documentation/hybrid-cloud/operator-configuration.md:133`; `documentation/hybrid-cloud/operator-configuration.md:194`)

The open UI reflects this boundary, resharding controls depend on `reshardingEnabled`, and progress polling reads a `_cluster_manager/resharding/progress/<collection>` metadata key reported by a manager.
UI support consequently does not prove a self-contained open-source resharding controller exists. (`qdrant-web-ui/src/components/Collections/ClusterMonitor/ReshardingButtons.jsx:25`; `qdrant-web-ui/src/components/Collections/ClusterMonitor/reshardingProgress.js:8`)

Payload tenancy, custom shard keys, fallback routing, filtered tenant promotion, listener replicas, manual transfers and read/write consistency controls have implementations in the supplied engine, no Cloud gate was found in their inspected paths.
That conclusion is scoped to these features and paths, it is not a licensing inventory of every Qdrant product. (`src/common/collections.rs:382`; `lib/collection/src/shards/shard_holder/mod.rs:824`; `lib/collection/src/shards/replica_set/update.rs:145`; `lib/collection/src/shards/replica_set/execute_read_operation.rs:48`)

# 6. Design implications for nocturnum

## Concepts to retain

**(inferred)** Retain logical shards independent of replica placement, typed lifecycle transitions, an explicit metadata command log and a common local/remote shard interface.
Qdrant demonstrates why lifecycle state needs separate readable, writable and authoritative predicates, while persisted transfer records make migrations recoverable across process restarts. (`lib/collection/src/shards/replica_set/replica_set_state.rs:135`; `lib/collection/src/shards/shard_trait.rs:87`; `lib/collection/src/shards/transfer/mod.rs:102`)

**(inferred)** Retain idempotent metadata application with completion markers written last, resolve random placement and defaults before proposing commands, and make snapshot installation’s crash ordering part of the storage specification.
The optional planner’s replay rules and existing snapshot recovery windows provide concrete behavior to specify independently for nocturnum. (`lib/storage/src/content_manager/consensus_state_machine/mod.rs:10`; `lib/storage/src/dispatcher.rs:87`; `lib/storage/src/content_manager/consensus_manager.rs:729`)

**(inferred)** Retain shared tenant storage with indexed tenant filters and optional physical grouping, but expose tenant identity as a mandatory authenticated request scope.
Filtering must cover reads, ID-based retrieval, writes, deletes and administrative operations, Qdrant’s optional selector and optimization flag should not become nocturnum’s security boundary. (`lib/collection/src/shards/shard_holder/mod.rs:696`; `lib/shard/src/optimize.rs:226`; `lib/storage/src/rbac/mod.rs:45`)

## Simplifications and different choices

**(inferred)** Choose one documented point-consistency model initially, for example a leader per shard with explicit fencing and replicated sequence numbers, rather than reproducing weak, medium and strong coordination plus multiple clock families.
Qdrant’s leader-selection changes, partial-write outcomes and cutoff bookkeeping show the verification burden of exposing several semantics simultaneously. (`lib/collection/src/shards/replica_set/update.rs:218`; `lib/collection/src/wal_delta.rs:14`)

**(inferred)** Separate acceptance, replication, application, visibility and durable flush in nocturnum’s response contract, validate the write threshold even when no attempted replica fails, and define retry IDs and uncertain outcomes.
These choices directly address the inspected threshold branch and the distinction between mmap WAL append and durable flush. (`lib/collection/src/shards/replica_set/update.rs:541`; `lib/collection/src/shards/shard_trait.rs:23`; `lib/wal/src/segment.rs:331`)

**(inferred)** Prefer explicit partial-query policy and version-aware replica reconciliation, initially use full per-shard candidate limits and deterministic top-k merging.
Delay probabilistic undersampling and speculative fan-out until their effect on recall, consistency and resource use is measurable, since Qdrant’s implementation accepts candidate shortfalls and compares scores during reconciliation. (`lib/collection/src/collection/query.rs:71`; `lib/collection/src/shards/resolve.rs:176`)

## Minimal v1 and later phases

1. **Minimal v1, (inferred):** a single-node engine with stable logical shard IDs, fixed shard count, enforced tenant scope, payload indexes, explicit WAL durability and exportable snapshots, define distributed command and wire formats without claiming high availability.
2. **First distributed phase, (inferred):** metadata Raft, three-voter deployment support, manual replica placement, one write-ordering model, authenticated peer RPC, snapshot bootstrap with ordered catch-up, node drain/removal and observable recovery states, ship failure-injection coverage before automatic balancing.
3. **Later phases, (inferred):** delta recovery, tenant promotion, online resharding, hedged reads, failure-domain-aware balancing and an open operator, use routing epochs and independently checkable migration completion criteria to limit stale-coordinator behavior.

This ordering follows the dependency visible in Qdrant, recovery and migration already depend on metadata synchronization, while promotion and resharding add further routing-state transitions. (`lib/collection/src/shards/transfer/driver.rs:137`; `lib/collection/src/shards/transfer/stream_records.rs:178`; `lib/collection/src/shards/resharding.rs:75`)

## Candidate Rust crates

These are **(inferred)** candidates drawn from observed source dependencies, suitability and licenses should be assessed during nocturnum implementation.

| Area | Candidates and source evidence |
|---|---|
| Consensus | `raft` from TiKV `raft-rs`, already exercised through `RawNode` and an application-owned `Storage` implementation. (`Cargo.toml:268`; `lib/storage/src/content_manager/consensus_manager.rs:1430`) |
| Async transport and coordination | `tokio`, `tonic`, `prost`, `futures`, `tokio-util`, supporting RPC, bounded channels, cancellation and concurrent collection. (`Cargo.toml:303`; `Cargo.toml:305`; `lib/collection/src/shards/transfer/driver.rs:257`) |
| Persistence and synchronization | `serde`, `serde_json`, a versioned binary serialization choice, `atomicwrites`, `parking_lot`, keep persisted format design independent of Rust enum layout. (`lib/storage/src/content_manager/consensus/persistent.rs:8`; `lib/storage/src/content_manager/mod.rs:74`) |
| WAL primitives | `memmap2`, `crc32c`, implement an independent WAL specification with tested flush and directory-persistence ordering. (`lib/wal/Cargo.toml:18`; `lib/wal/Cargo.toml:22`; `lib/wal/src/segment.rs:125`) |
| Routing and identity | `hashring`, `siphasher`, `uuid`, or an independently specified stable routing algorithm, preserve algorithm/version identity across restarts and upgrades. (`lib/collection/Cargo.toml:51`; `lib/collection/Cargo.toml:53`; `lib/collection/src/shards/resharding.rs:6`) |

# 7. Open questions

1. **Does the inferred write-threshold bypass reproduce?** Test two configured replicas with one already Dead, write-consistency factor two and a successful write to the remaining Active copy, compare this with a replica failing during the request. (`lib/collection/src/shards/replica_set/update.rs:343`; `lib/collection/src/shards/replica_set/update.rs:541`)
2. **What exact loss window follows acknowledgement?** Trace process-crash versus machine-power-loss behavior for WAL, segment and visible waits, the inspected append path has no per-request flush and the worker flushes periodically. (`lib/wal/src/segment.rs:331`; `lib/collection/src/update_workers/flush_workers.rs:149`)
3. **What fencing does nocturnum require?** Decide whether disconnected coordinators may accept writes, whether shard epochs must reject them, and how acknowledged divergent writes reconcile after partition, Qdrant’s metadata majority does not itself answer those point-level questions. (`lib/collection/src/shards/replica_set/update.rs:218`; `documentation/scaling/horizontal-scaling.md:81`)
4. **Which resharding invariants are enforced by the external manager?** Establish migration-completeness checks, cleanup ordering, restart behavior and rollback boundaries before specifying an open controller, the in-engine driver call is disabled. (`lib/collection/src/collection/resharding.rs:140`; `lib/collection/src/shards/resharding.rs:75`)
5. **What tenant boundary will v1 guarantee?** Decide payload-scoped versus physically separate tenants, cross-tenant ID uniqueness, schema sharing, quotas and authorization for administrative APIs, the source supports routing and grouping but does not make those choices equivalent. (`lib/storage/src/rbac/mod.rs:45`; `lib/collection/src/collection/search.rs:318`; `documentation/manage-data/multitenancy.md:15`)