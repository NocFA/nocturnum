# How Qdrant works

A plain-language synthesis of how Qdrant 1.19.2 works underneath, written as the groundwork for nocturnum.

It draws on five source-level reports in [`research/`](research/), each written from a reading of the Qdrant source, web UI and docs (Qdrant 1.19.2, read from separate read-only clones), with every claim there cited to a file and line. Where the docs and the source disagree, the reports trust the source, and so does this page.

| Report | Covers |
|---|---|
| [a1-storage](research/a1-storage.md) | WAL, segments, ID trackers, vector and payload storage, the optimizer, flushing, snapshots |
| [a2-index](research/a2-index.md) | HNSW, filterable HNSW and ACORN, the query planner, quantization, distance maths, sparse and multivectors, GPU builds |
| [a3-query](research/a3-query.md) | payload model, payload indexes, the filter language, cardinality estimation, the universal Query API, fusion maths, BM25 |
| [a4-distributed](research/a4-distributed.md) | Raft, shards, replicas, transfers, consistency, resharding, multi-tenancy |
| [a5-surface](research/a5-surface.md) | REST and gRPC, auth, config, metrics, the web UI, Edge, licensing, what is Cloud-only, client compatibility |

## The mental model

A Qdrant **collection** holds **points**, each point is an ID (an unsigned 64-bit integer or a UUID), one or more named vectors and a JSON payload.

You search a collection by giving it a query vector, it returns the closest points by a distance metric (cosine, dot product, Euclidean or Manhattan), optionally restricted by a filter over the payload.

Underneath, a collection is split into **shards**, each shard is a small database of its own with a write-ahead log and a set of **segments**, and each segment holds a slice of the points with their vectors, payloads and indexes. New writes land in small mutable segments, and a background **optimizer** keeps rebuilding them into large immutable segments with an HNSW graph on top, so search stays fast while writes keep flowing.

Everything else, the filters, quantization, hybrid queries, replication and the console, is layered on that core.

## The write path

1. A request (REST or gRPC) is turned into an operation and routed to the shard that owns each point ID, by hashing the ID onto a ring.
2. The shard checks there is disk headroom, reserves a slot in a bounded update queue (200 operations by default), then appends the operation to its **WAL** under a mutex, so queue order always matches log order.
3. Depending on the `wait` setting, the client gets an answer right after the append, after the operation is applied to segments, or after it is visible to search.
4. A single update worker per shard applies operations in order to the segments.
5. A flush worker runs every 5 seconds: it flushes the WAL, then each segment's parts in a fixed order (ID mappings, vectors, payload indexes and payloads, point versions, segment state), and only then lets the WAL be truncated up to what is now durable.

Two details matter for correctness:

- **Every point carries a version**, the sequence number of the last operation that touched it, so replaying the WAL after a crash is safe, an operation older than the point's version is skipped.
- **Filter-based updates are resolved before they are logged.** "Delete everything where `colour = red`" is turned into a concrete list of IDs first and that list goes into the WAL, so replay deletes the same points even if the payloads have changed since.

A sharp edge the storage report found: the docs say anything in the WAL survives power loss, but the WAL append writes to mapped memory and only becomes durable at the next flush, so the honest contract is weaker than the documented one. nocturnum should state its acknowledgement levels plainly and test them with crash tests.

## Segments and the optimizer

A segment holds an **ID tracker** that maps external IDs to compact internal `u32` offsets, **vector storage** per named vector, **payload storage**, **payload indexes** and, once built, a **vector index**.

Segments come in two kinds:

- **Appendable** segments use a plain (brute-force) vector index and chunked storage that can grow, so they accept inserts.
- **Non-appendable** segments carry an HNSW graph and contiguous storage, they still accept deletes and payload-index changes, but changing a point there means copying its latest version into an appendable segment and marking the old copy deleted, with a recorded rule that the new copy must be durable before the deletion is.

The optimizer has four jobs, picked by thresholds:

- **Indexing** turns a segment that has grown past `indexing_threshold` (10,000 KB of vectors by default) into an HNSW-indexed one.
- **Merge** combines small segments, aiming for `default_segment_number` (CPU count divided by two, clamped to 2 to 8).
- **Vacuum** rebuilds segments where more than 20% of points are deleted.
- **Config mismatch** rebuilds segments whose settings no longer match the collection.

While it works, the optimizer wraps the source segments in **proxy segments**, which keep answering reads and redirect writes to a fresh appendable segment. The new segment is built off to the side, flushed, marked ready with a `version.info` file, and only then swapped in, and the old files are deleted after the next durable flush. That build-aside, publish, then reclaim sequence is the most important idea to copy.

## Storage formats

- **WAL:** segment files of 32 MB, each record is a length, a CBOR-encoded operation and a CRC32C chained from a random per-file seed, so stale bytes in a reused file never pass the check. Recovery stops at the first bad record.
- **Dense vectors:** appendable storage is a series of 32 MB mmap chunk files, immutable storage is one contiguous file of fixed-width records. Types are float32, float16 and uint8, and memory placement (`cached` or `cold`) is now separate from the on-disk format.
- **Payloads and sparse vectors:** stored in Qdrant's own **Gridstore**, a block allocator over 32 MB pages with 128-byte blocks, LZ4 compression, a pointer journal and free-space summaries. RocksDB is gone, replaced because of LSM compaction latency spikes and tuning overhead, and the old format no longer loads.
- **Snapshots:** tar archives of every segment plus collection config, created while writes continue (the segments are proxied for the duration) and written with a SHA-256 sidecar. They are node-local and can be stored on disk, S3, GCS or Azure.

## The vector index

Qdrant uses **HNSW**, a layered proximity graph, one per named vector per segment.

- Each point gets a random top layer, roughly `round(-ln(U) / ln(m))`. Upper layers keep `m` links per point (16 by default) and the bottom layer keeps `2m`.
- Inserting a point walks down from the entry point greedily, runs a beam search of width `ef_construct` (100 by default) on each shared layer, and keeps a diverse set of neighbours: a candidate is dropped if it is closer to an already chosen neighbour than to the new point.
- Search descends the upper layers greedily, then runs a beam search of width `max(ef, limit)` on the bottom layer.
- Building is parallel with Rayon after a sequential seed of 256 points, and can run on a GPU through Vulkan.

**Filtering is where Qdrant earns its keep.** A filter can cut the graph into disconnected islands, so Qdrant does three things:

1. **Payload-aware links:** for each indexed payload value, it builds a small graph over just the points with that value (using `payload_m`) and merges those links into the main graph, so "points with `city = Dublin`" stay connected among themselves.
2. **A query planner** estimates how many points match the filter (a minimum, expected and maximum count from the payload indexes). If few match, it lists them from the payload index and scores them directly. If many match, it walks the graph and skips non-matching points. If the estimate is unclear, it samples up to 1,000 points and decides from a confidence interval.
3. **ACORN** (optional) lets a search step through non-matching neighbours to reach matching ones two hops away.

None of this guarantees perfect recall under filters, it is mitigation, which is why nocturnum should keep exact search as the reference every ANN result is checked against.

**Quantization** keeps a compressed copy of every vector for fast scoring, with the originals kept (often on disk) for rescoring:

- **Scalar** maps each float to one byte, about 4x smaller.
- **Product** splits vectors into sub-spaces with 256 learned centroids each, 4x to 64x smaller.
- **Binary** keeps 1, 1.5 or 2 bits per dimension (32x at one bit), with an asymmetric mode that scores a higher-precision query against compact stored codes.
- **TurboQuant** is a newer rotation-based low-bit scheme.

Search over quantized vectors fetches `oversampling × limit` candidates and rescores them with the originals. Rescoring can't recover a point that wasn't fetched in the first place.

For scale: one million 768-dimension float32 vectors take 3.07 GB raw, about 772 MB scalar-quantized and 96 MB at one bit, while the HNSW bottom layer at `m = 16` adds roughly 128 MB of links, so after heavy quantization the graph becomes a large share of memory.

**Sparse vectors** (for keyword-style retrieval such as BM25 or SPLADE) use an inverted index with optional IDF weighting, and **multivectors** (ColBERT-style token vectors) score with MaxSim, the sum over query tokens of each one's best match in the document.

## Payloads, filters and queries

Payloads are arbitrary JSON. A filter combines `must` (all true), `should` (at least one), `must_not` (none) and `min_should` (at least N), and conditions can nest. Condition types include exact match, match-any and except, ranges over numbers and datetimes, full-text and phrase, prefix, `is_empty`, `is_null`, `values_count`, `has_id`, `has_vector`, `nested` (all sub-conditions must hold on the same array element) and geo box, radius and polygon.

Array fields match if any element matches, and missing, null and empty array are three different states.

**Payload indexes** are opt-in per field: keyword, integer (lookup and range as separate structures), float, bool, geo, datetime, UUID and full-text with tokenizers, stemmers, stopwords and phrase support. They speed up filtering, feed the planner's cardinality estimates and drive the payload-aware HNSW links, which is why Qdrant says to create them before loading data. `is_tenant` and `is_principal` flags also tell the optimizer to lay points out on disk grouped by that field.

The **universal Query API** (`/points/query`) is a tree: leaf **prefetches** fetch candidates (vector search, scroll, `order_by`, random sample), and the root re-scores or fuses them. Root options include:

- **RRF** fusion, `1 / (rank + k)` summed per point with `k = 2`.
- **DBSF** fusion, which normalises each list's scores by mean ± 3 standard deviations and sums them.
- **Formula** scoring with payload variables, conditions, decay functions and geo distance.
- **Recommend**, **discover** and **context** queries from example points.
- **MMR** for diverse results.

Group-by, facets, count, scroll and a distance matrix sit alongside it. The query report has the exact maths.

A built-in **BM25** path can turn a text field into a sparse vector on the server, which is separate from the full-text filter.

## Distributed mode

- **Raft** (the TiKV `raft` crate) is used only for cluster metadata: membership, collection configs, aliases, shard placement and transfers. Point writes never go through it.
- Each logical shard has a **replica set** spread across peers, replicas move through states such as Active, Dead, Partial, Initializing, Listener, Recovery and Resharding.
- Writes go straight to the replicas. `write_consistency_factor` sets how many must succeed, and the `ordering` setting (`weak`, `medium` or `strong`) decides whether writes funnel through one coordinating replica. A failed write is never rolled back, so some replicas may already hold it.
- Shard **transfers** copy data between peers by streaming records, sending a snapshot (the current default, although the docs still say streaming), or replaying a WAL delta.
- Search fans out to one replica per shard and merges the top results.
- **Custom sharding** routes by a shard key, which is how a large tenant gets its own shard.

The distributed report flags one suspected bug worth a test if nocturnum goes there: the write-consistency check looks like it can pass when dead replicas are skipped.

## The surface

- **REST** (Actix Web) and **gRPC** (Tonic) expose the same operations through one internal dispatcher. The OpenAPI file is assembled from handwritten templates plus generated schemas, and there is a separate internal gRPC for peers.
- **Auth:** an admin API key, a read-only key, and JWTs signed with the key that carry per-collection read or write access. TLS, audit logs and Prometheus `/metrics` are all in the open build.
- The default config binds every interface with CORS on and no auth, so a fresh install is wide open.
- The **web UI** is a separate React and MUI app served as static files from `/dashboard`. It covers collections, a points browser and editor, an API console, a search quality check that measures ANN recall against exact search, snapshots, a graph explorer, a dimensionality-reduction visualiser, tutorials, sample datasets, a JWT generator and quotas.
- **Qdrant Edge** is an embedded, in-process build of the engine without the server.

## What is open and what is Cloud-only

The engine, web UI and Edge are all Apache-2.0, and the open build has no enterprise feature gate. What Qdrant keeps for its Cloud, Hybrid Cloud and Private Cloud products is the operations around the engine:

- scheduled backups with retention, and disk-level restore,
- monitoring dashboards, log views and alerting,
- automatic shard rebalancing, replication-factor repair, node replacement, rolling upgrades and zone-aware placement,
- **automated resharding** (the engine has the parts, but the open build rejects starting it and the driver call is commented out),
- account-level access control and billing,
- hosted inference (OpenAI, Cohere, Jina, OpenRouter proxies).

That list is nocturnum's opportunity: the engine ideas are well understood and openly documented, and the gap a self-hoster feels is the operations layer.

## Sharp edges worth not copying

- Documented durability is stronger than the code's.
- A field condition with several predicates is checked as OR, which surprises people expecting AND, and an empty `should` behaves differently in two evaluators.
- Fusion results depend on candidate depth and on whether fusion runs per shard or at the root, and prefetch limits silently cap pagination.
- Merged payload links can push graph degree past `m`, so memory estimates from `m` alone are low.
- Docs drift: the default transfer method, HNSW level rounding and non-appendable segment rules all differ between docs and source.

## What this suggests for nocturnum v1

The five reports agree on a shape. These are proposals, and the owner decides scope:

1. **Single node first**, with stable shard IDs in the format so clustering can come later without a rewrite.
2. **Exact search as the reference**, then one CPU HNSW implementation that is checked against it on every test run.
3. **A simple, honest storage core:** one WAL with checksums and explicit fsync levels, an append-only record log, immutable indexed generations published through a manifest, and crash tests at every publish and reclaim step.
4. **A small payload index set** (keyword, integer, float, bool, null and presence), a cost-based planner choosing between listing filter matches and walking the graph, and one reference filter evaluator that defines the semantics.
5. **Scalar quantization with rescoring** as the first compression, everything fancier later.
6. **The operations Qdrant keeps for its Cloud, built in:** scheduled snapshots with retention, metrics and alerts, and keys with per-collection scope, all managed from the console.
7. **API:** the surface report recommends implementing a declared subset of Qdrant's REST and gRPC API wire-compatibly, so existing `qdrant-client` libraries and LangChain integrations work unchanged, plus an nocturnum admin API. The owner chose to decide this after reading the research.

## What the owner decided

The owner settled v1 scope on 2026-10-10, recorded in [`decisions/0001-v1-scope.md`](decisions/0001-v1-scope.md): nocturnum has its own API rather than Qdrant wire compatibility, v1 is single-node, writes are acknowledged once durable by default, and the ports are 6333 for REST and 6334 for gRPC.
