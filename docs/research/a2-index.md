# 1. Summary

- **Qdrant’s dense index is a segment-local, filterable HNSW graph over internal point IDs**, with one independent index and storage configuration per named vector space, appendable segments handle incoming changes while optimized HNSW files remain immutable. `Segment`, `VectorData`, `VectorIndexRead`, `VectorIndex`, and `HNSWIndex` form the main boundaries. Sources: `lib/segment/src/segment/mod.rs:65`, `lib/segment/src/segment/mod.rs:108`, `lib/segment/src/index/vector_index_base.rs:28`, `lib/segment/src/index/hnsw_index/hnsw/vector_index_impl.rs:79`.

- **Filtering changes both graph construction and query planning**, indexed payload blocks contribute additional links, small estimated result sets use payload enumeration plus vector scoring, large sets use filtered HNSW, and uncertain estimates trigger sampling. ACORN optionally traverses through rejected neighbors to reach matching second-hop neighbors, these mechanisms reduce disconnection without guaranteeing exhaustive recall. Sources: `lib/segment/src/index/hnsw_index/hnsw/build/additional_links.rs:37`, `lib/segment/src/index/hnsw_index/hnsw/read_view/dispatch.rs:106`, `lib/segment/src/index/hnsw_index/graph_layers.rs:159`.

- **Quantization is an additional scoring/storage layer**, scalar, product, binary and TurboQuant representations can drive graph construction and search, oversampling increases the candidate set and rescoring reads original storage. Binary supports 1, 1.5 and 2 bits per coordinate plus higher precision asymmetric queries, TurboQuant provides another distinct family at 1, 1.5, 2 and 4 bits. Sources: `lib/segment/src/vector_storage/quantized/quantized_vectors.rs:146`, `lib/segment/src/index/vector_index_search_common.rs:15`, `lib/quantization/src/encoded_vectors_binary.rs:34`, `lib/segment/src/types.rs:1083`.

- **Dense, multivector and sparse retrieval share result plumbing but use different algorithms**, dense HNSW is approximate, multivector MaxSim sums query-token best matches, sparse search traverses inverted postings and optionally applies query-side IDF computed across segments of a shard. SIMD dispatch depends on datatype and encoding, GPU construction uses Vulkan rather than CUDA. Sources: `lib/segment/src/vector_storage/query_scorer/mod.rs:74`, `lib/sparse/src/index/search_context.rs:42`, `lib/shard/src/query/query_context.rs:75`, `lib/gpu/src/instance.rs:98`.

- **For nocturnum, the strongest conceptual foundation is exact search plus immutable indexed segments and explicit scoring contracts**, followed by one HNSW implementation, measured filter planning and scalar quantization, more complicated compression, multivector ANN and GPU construction belong in later phases. This is a design recommendation, inferred from the implementation boundaries and interactions described below.

# 2. How it works, fundamentally

Paths beginning `lib/`, `src/` or `config/` are relative to the root of the `qdrant/qdrant` repository, whose package manifest declares version `1.19.2` at `Cargo.toml:3`.

Documentation paths are relative to `qdrant-landing/content/documentation` in the `qdrant/landing_page` repository.

This report comes from static source and documentation inspection, no builds, executions or performance measurements were performed.

## Segment and vector-space boundaries

`Segment` owns an ID tracker, payload index and a map from vector names to `VectorData`, each `VectorData` contains its own vector index, original storage and optional quantized storage. Named vectors therefore represent independent search spaces attached to shared point identities, querying a name selects its corresponding index rather than concatenating representations. Sources: `lib/segment/src/segment/mod.rs:65`, `lib/segment/src/segment/mod.rs:81`, `lib/segment/src/segment/mod.rs:108`, `lib/segment/src/segment/read_view/search.rs:386`.

`VectorIndexRead::search` accepts a batch of query vectors, an optional filter, a requested result count, search parameters and `VectorQueryContext`, returning internal-ID scored results per query. `VectorIndex` adds file enumeration and mutation operations, `VectorIndexEnum` selects plain, HNSW and several sparse implementations. Source: `lib/segment/src/index/vector_index_base.rs:28`.

HNSW mutation explicitly returns an error, its files are immutable, so updates must pass through appendable segment/storage machinery and later index rebuilding. The optimizer’s indexing threshold controls when an indexed segment becomes worthwhile, this threshold is separate from the HNSW query planner’s scan threshold. Sources: `lib/segment/src/index/hnsw_index/hnsw/vector_index_impl.rs:79`, `lib/shard/src/optimizers/config.rs:17`, `lib/shard/src/optimizers/segment_optimizer.rs:218`.

## HNSW graph structure and insertion

`HnswGraphConfig` derives base-layer connectivity `m0 = 2m`, upper-layer connectivity `m`, and corresponding payload values `payload_m0 = 2payload_m`. `GraphLayers` holds graph links, entry points and a reusable visited-list pool, `GraphLayerData` persists structural parameters and entry points. Sources: `lib/segment/src/index/hnsw_index/config.rs:12`, `lib/segment/src/index/hnsw_index/config.rs:45`, `lib/segment/src/index/hnsw_index/graph_layers.rs:63`.

The builder chooses a point’s highest level using approximately

\[
L=\operatorname{round}\left(\frac{-\ln U}{\ln(\max(m,2))}\right).
\]

The implementation uses rounding, which differs from descriptions that use flooring, and clamps the random value away from zero to prevent an effectively unlimited level allocation. Sources: `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:322`, `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:389`.

Insertion begins from a usable entry point, greedily descends levels above the new point’s level, then runs a construction beam search at shared levels. It selects diversified neighbors and inserts backlinks, the diversification heuristic rejects a candidate that is closer to an already selected neighbor than to the point being inserted. Equal-distance handling deliberately avoids a duplicate-vector clique becoming a connectivity sink. Sources: `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:429`, `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:514`, `lib/segment/src/index/hnsw_index/links_container.rs:9`.

`GraphLayersBuilder` uses a `RwLock<LinksContainer>` for each point/layer, a mutex for entry points, atomic readiness and maximum-level state, and a visited pool. Backlinks acquire individual neighbor locks, construction does not hold one global graph lock across every insertion. Sources: `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:30`, `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:514`.

CPU construction seeds the graph sequentially before parallel insertion, the release build’s bootstrap is 256 points, the debug build’s is 32. Remaining points run through a dedicated Rayon pool with bounded job length, existing indexed data can supply an old graph and healing/remapping input. Quantized scoring is used during construction when available. Sources: `lib/segment/src/index/hnsw_index/hnsw.rs:35`, `lib/segment/src/index/hnsw_index/hnsw/build.rs:98`, `lib/segment/src/index/hnsw_index/hnsw/build.rs:283`, `lib/segment/src/index/hnsw_index/hnsw/build/main_graph.rs:48`, `lib/segment/src/index/hnsw_index/hnsw/build/main_graph.rs:93`.

Search greedily descends upper layers, then performs a base-layer beam search using a candidate heap and a bounded nearest-results queue. Expansion stops when the best remaining candidate falls below the current lower bound, the base-layer beam is at least `max(hnsw_ef, requested_top)`. Filtering and deletion checks precede scoring, ordinary expansion scores at most the layer’s connectivity limit after filtering. Sources: `lib/segment/src/index/hnsw_index/graph_layers.rs:116`, `lib/segment/src/index/hnsw_index/graph_layers.rs:267`, `lib/segment/src/index/hnsw_index/graph_layers.rs:537`, `lib/segment/src/index/hnsw_index/point_scorer.rs:266`.

## Filterable HNSW and ACORN

Additional graph construction enumerates payload blocks from indexed fields, builds temporary block-restricted graphs using `payload_m`, then merges their links into the main graph. Fields can disable this contribution through `enable_hnsw`, setting `payload_m=0` disables additional payload links. Sources: `lib/segment/src/index/hnsw_index/hnsw/build/additional_links.rs:37`, `lib/segment/src/index/hnsw_index/hnsw/build/additional_links.rs:203`.

Construction avoids blindly building every block, it skips very small blocks, skips sufficiently large blocks when a main graph exists, and uses sampled connectivity estimates to avoid redundant graphs for later non-tenant fields. Tenant fields receive different treatment, the exact conditions depend on block size, existing connectivity and the scan threshold. Source: `lib/segment/src/index/hnsw_index/hnsw/build/additional_links.rs:90`.

Merging deduplicates adjacency entries but does not prune the combined list back to `m` or `m0`, actual stored degrees can therefore exceed those limits. Ordinary search filters the complete adjacency list before applying its scoring limit, payload-tail neighbors can become useful when earlier neighbors fail the filter. Sources: `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:348`, `lib/segment/src/index/hnsw_index/graph_layers.rs:137`, `lib/segment/src/index/hnsw_index/point_scorer.rs:266`.

Entry selection tries supplied entries and stored graph entries, then can score filtered points as multiple base-layer seeds, the target seed count is twice `max(ef, top)`. This helps when no high-level entry satisfies a filter, it does not establish connectivity between every matching component. Sources: `lib/segment/src/index/hnsw_index/graph.rs:67`, `lib/segment/src/index/hnsw_index/graph.rs:163`, `lib/segment/src/index/hnsw_index/graph.rs:219`.

ACORN maintains separate visitation state for ordinary expansion and bridge exploration, it can inspect neighbors of filtered-out or deleted neighbors without scoring those bridge points. Second-hop exploration is bounded, including sampling from additional payload links, this provides another route through fragmented filtered graphs. It activates only when explicitly enabled, main base-layer connectivity exists, and estimated selectivity is within the configured threshold. Sources: `lib/segment/src/index/hnsw_index/graph_layers.rs:159`, `lib/segment/src/index/hnsw_index/hnsw/read_view/dispatch.rs:222`.

Documentation describes payload-aware links and warns about difficult filter combinations and deletion-induced disconnection, source confirms mitigation rather than an exhaustive-search guarantee. Documentation’s simplified “maximum degree `m`” description omits `m0=2m` and merged payload tails. Documentation: `manage-data/indexing.md:284`, `manage-data/indexing.md:320`.

## Query planning

Without a filter, exact mode, disabled graph construction or a sufficiently small live vector population chooses plain search, otherwise the index chooses graph search. Exact mode also disables quantized scoring. Source: `lib/segment/src/index/hnsw_index/hnsw/read_view/dispatch.rs:36`.

With a filter, the payload index produces minimum, expected and maximum cardinality estimates, these are adjusted for vector availability because a matching point need not have the requested named vector. If the maximum is below the scan threshold, Qdrant enumerates matching points and scores them, if the minimum is above it, Qdrant searches the filtered graph. Ambiguous estimates trigger sampling. Sources: `lib/segment/src/index/hnsw_index/hnsw/read_view/dispatch.rs:106`, `lib/segment/src/index/query_estimator.rs:14`.

Sampling uses up to 1,000 points and Agresti–Coull confidence bounds with `z=2`, deciding early when an interval lies clearly above or below the threshold. The dense threshold is configured in KB and converted to a point count using average original-vector bytes, multivector size therefore affects the effective threshold. Sources: `lib/segment/src/index/sample_estimation.rs:5`, `lib/segment/src/index/hnsw_index/hnsw.rs:189`.

“Payload-first” does not necessarily mean cheap enumeration, indexed primary clauses supply candidate iterators, but absent suitable clauses the payload index scans IDs and checks conditions. The expected cardinality adjustment also assumes vector availability is uncorrelated with filter matches, this is a planner estimate rather than a statistical invariant. Sources: `lib/segment/src/index/struct_payload_index/read_view/payload_index_read.rs:173`, `lib/segment/src/index/query_estimator.rs:14`.

## Graph and vector files

Graph metadata lives in `graph.bin`, adjacency can live in legacy `links.bin`, compressed `links_compressed.bin` or inline-vector `links_comp_vec.bin`, configuration lives in `hnsw_config.json`. New builds choose compressed storage, loading prefers inline compressed, then compressed, then legacy plain files. Sources: `lib/segment/src/index/hnsw_index/graph_layers.rs:55`, `lib/segment/src/index/hnsw_index/config.rs:9`, `lib/segment/src/index/hnsw_index/hnsw/build.rs:315`, `lib/segment/src/index/hnsw_index/graph_layers.rs:649`.

Compressed headers contain point/layer counts, connectivity parameters, neighbor-region sizes and bit-packed-offset metadata. Base-layer adjacency follows internal point order, upper-layer addressing uses a reindex ordered by point level. The first main-connectivity neighbors are sorted and delta encoded with a local bit width, additional payload-tail links use a global width and retain their order, link widths have a minimum of eight bits. Sources: `lib/segment/src/index/hnsw_index/graph_links/header.rs:25`, `lib/segment/src/index/hnsw_index/graph_links/serializer.rs:51`, `lib/common/common/src/bitpacking_links.rs:6`.

Immutable dense storage uses `matrix.dat` and `deleted.dat`, the matrix has a four-byte header followed by fixed-stride vector values. Appendable mmap multivectors instead maintain flattened rows plus `{offset, count, capacity}` records, each record uses three `u32` fields. Sources: `lib/segment/src/vector_storage/dense/dense_vector_storage.rs:32`, `lib/segment/src/vector_storage/dense/immutable_dense_vectors.rs:25`, `lib/segment/src/vector_storage/multi_dense/appendable_mmap_multi_dense_vector_storage.rs:38`.

Graph memory placement distinguishes cold mmap, cached mmap and pinned heap representations, low-memory/load profiles can reduce requested residency. Linux additionally supports a batched `io_uring` graph path, other platforms use mmap. Inline storage puts an original vector at its base-layer node and quantized neighbor copies beside adjacency, trading disk space for fewer separate reads, it requires quantization and excludes multivectors. Sources: `lib/segment/src/index/hnsw_index/hnsw.rs:218`, `lib/segment/src/index/hnsw_index/graph.rs:28`, `lib/segment/src/index/hnsw_index/graph_links/view.rs:77`, `lib/segment/src/types.rs:849`.

## Distance math and SIMD

All internal search queues compare scores with larger values preferred:

| Metric | Internal pair score | Nearest-query output |
|---|---|---|
| Dot | \(\sum_i x_i y_i\) | Unchanged |
| Cosine, ordinary float storage | Dot product after preprocessing | Unchanged |
| Euclidean | \(-\sum_i(x_i-y_i)^2\) | \(\sqrt{|s|}\) |
| Manhattan | \(-\sum_i|x_i-y_i|\) | \(|s|\) |

Sources: `lib/segment/src/spaces/simple.rs:36`, `lib/segment/src/spaces/simple.rs:80`, `lib/segment/src/spaces/simple.rs:124`.

Cosine normalization occurs when preparing stored vectors and queries, already normalized and near-zero vectors skip normalization. The near-zero test uses squared norm below `f32::EPSILON`, normalized tolerance is `1e-6`, so extremely small vectors retain their original magnitude. Named-vector preprocessing applies independently to each multivector row. Sources: `lib/segment/src/spaces/simple.rs:228`, `lib/segment/src/spaces/tools.rs:14`, `lib/segment/src/data_types/named_vectors.rs:311`.

Storage supports float32, float16 and uint8, this checkout also includes Turbo4. Float16 converts through half precision, uint8 conversion uses Rust’s numeric cast rather than learning an automatic calibration range. Uint8 cosine leaves stored values unchanged and computes norms during comparison, returning zero for a zero norm. Sources: `lib/segment/src/types.rs:2054`, `lib/segment/src/data_types/primitive.rs:76`, `lib/segment/src/data_types/primitive.rs:126`, `lib/segment/src/spaces/metric_uint/simple_cosine.rs:53`.

SIMD dispatch is encoding-specific, ordinary float32 dot scoring selects AVX plus FMA, SSE or NEON before scalar fallback, float16’s AVX route also requires F16C, uint8 uses AVX2. Binary popcount has an AVX-512VL/VPOPCNTDQ route, TurboQuant has AVX-512 VNNI, AVX2, SSE and NEON variants. AVX-512 availability does not imply every distance kernel uses it. Sources: `lib/segment/src/spaces/simple.rs:129`, `lib/segment/src/spaces/metric_f16/simple_dot.rs:25`, `lib/segment/src/spaces/metric_uint/simple_dot.rs:24`, `lib/quantization/src/encoded_vectors_binary.rs:292`, `lib/quantization/src/turboquant/simd/mod.rs:49`.

Nearest-query scores are postprocessed after segment result aggregation, custom recommendation/discovery/context scores bypass that conversion. Source: `lib/collection/src/shards/local_shard/search.rs:160`.

## Quantized representations and rescoring

`QuantizedVectors` separates configuration and encoded storage from original vector storage, persisted artifacts include `quantized.config.json`, `quantized.data`, `quantized.meta.json` and optional offsets. Query behavior is mediated by encoded scorers, originals remain available independently. Sources: `lib/segment/src/vector_storage/quantized/quantized_vectors.rs:52`, `lib/segment/src/vector_storage/quantized/quantized_vectors/config.rs:10`.

**Scalar quantization** exposes an int8 configuration, but the inspected encoder maps values into physical byte codes spanning `0..127`, with shared range metadata and per-vector score correction. Optional quantile clipping fits a narrower range, physical vector length is dimension rounded to 16 plus four correction bytes. Sources: `lib/segment/src/types.rs:939`, `lib/quantization/src/encoded_vectors_u8.rs:85`, `lib/quantization/src/encoded_vectors_u8.rs:192`, `lib/quantization/src/encoded_vectors_u8.rs:622`.

**Product quantization** partitions coordinates into subspaces and learns 256 centroids per subspace, each vector stores one byte per subspace. Training uses a sample of up to 10,000 vectors and at most 100 k-means iterations, a query builds centroid-score lookup tables. Compression settings `x4/x8/x16/x32/x64` map to subspace widths `1/2/4/8/16` relative to float32 originals. Sources: `lib/quantization/src/encoded_vectors_pq.rs:28`, `lib/quantization/src/encoded_vectors_pq.rs:80`, `lib/segment/src/vector_storage/quantized/quantized_vectors.rs:236`.

**Binary quantization** has three storage encodings, one-bit stores signs, two-bit uses per-coordinate mean/standard-deviation statistics and ternary regions, 1.5-bit packs paired ternary information with a shared bit. The latter is a distinct lossy encoding, it is not simply two-bit data compressed without information loss. Physical lengths round to storage-word boundaries. Sources: `lib/quantization/src/encoded_vectors_binary.rs:34`, `lib/quantization/src/encoded_vectors_binary.rs:559`, `lib/quantization/src/encoded_vectors_binary.rs:592`, `lib/quantization/src/encoded_vectors_binary.rs:650`, `lib/quantization/src/encoded_vectors_binary.rs:886`.

Binary query encoding can match storage or use scalar4/scalar8, asymmetric queries use higher precision query bitplanes against the compact stored representation. Scoring combines bit operations across these planes, original database vectors remain compressed during candidate traversal. Sources: `lib/quantization/src/encoded_vectors_binary.rs:47`, `lib/quantization/src/encoded_vectors_binary.rs:72`, `lib/segment/src/vector_storage/quantized/quantized_vectors.rs:146`.

**TurboQuant** applies deterministic permutation/Hadamard rotation, distribution adjustment and low-bit encoding with error-correction metadata, its 1.5-bit mode differs from binary quantization’s paired encoding. This checkout builds the `Plus` mode, default depth is four bits. SIMD queries use higher precision integer representations, despite documentation describing full-precision query scoring, source does not perform plain float32 pair scoring in the main integer kernel. Sources: `lib/quantization/src/turboquant/rotation.rs:4`, `lib/quantization/src/encoded_vectors_tq.rs:148`, `lib/quantization/src/turboquant/encoding.rs:195`, `lib/segment/src/vector_storage/quantized/quantized_vectors/turbo/create.rs:37`, `lib/quantization/src/turboquant/simd/query/mod.rs:193`; documentation: `manage-data/quantization.md:60`.

Oversampling changes candidate count to approximately `floor(oversampling × top)`, rescoring replaces candidate scores with original-storage scores, sorts and truncates. Scalar/PQ default rescoring is false, binary default is true, TurboQuant defaults depend on bit depth. Rescoring cannot recover points absent from the candidate set, originals on disk introduce candidate-dependent I/O. Sources: `lib/segment/src/index/vector_index_search_common.rs:15`, `lib/segment/src/index/vector_index_search_common.rs:27`, `lib/segment/src/index/vector_index_search_common.rs:62`, `lib/segment/src/vector_storage/quantized/quantized_vectors/accessors.rs:16`.

## Multivectors and sparse retrieval

`MultiVectorConfig` currently exposes `MaxSim`, implemented as

\[
S(Q,D)=\sum_{q\in Q}\max_{d\in D}s(q,d).
\]

It is directional, each point remains one indexed object whose score involves its entire matrix, HNSW internal point-to-point comparisons can use the same matrix scorer. Quantized multivector scoring aggregates maxima without constructing the complete similarity matrix. Sources: `lib/segment/src/types.rs:2070`, `lib/segment/src/vector_storage/query_scorer/mod.rs:74`, `lib/segment/src/vector_storage/query_scorer/multi_metric_query_scorer.rs:113`, `lib/segment/src/vector_storage/quantized/quantized_multivector_storage/mod.rs:295`.

Sparse vectors contain unique dimension IDs and corresponding weights, ingestion sorts their indices, similarity is the dot product over intersecting dimensions. `SparseVectorIndex<InvertedIndex>` maps query dimensions to postings, mutable postings carry point ID, weight and suffix maximum weight. Immutable compressed postings bit-pack IDs in blocks of 128 and support float32, float16 or quantized uint8 weights. Sources: `lib/sparse/src/common/sparse_vector.rs:18`, `lib/sparse/src/common/sparse_vector.rs:67`, `lib/sparse/src/index/posting_list.rs:11`, `lib/sparse/src/index/compressed_posting_list.rs:18`, `lib/segment/src/index/vector_index_base.rs:110`.

Sparse search accumulates scores from query postings in ID batches, optional pruning requires nonnegative query weights and reliable suffix maxima, the compressed posting implementation reports that its suffix maxima are not reliable. A restrictive filter can choose candidate-first sparse scoring, unfiltered search always uses the inverted index. Sources: `lib/sparse/src/index/search_context.rs:42`, `lib/sparse/src/index/search_context.rs:145`, `lib/sparse/src/index/compressed_posting_list.rs:662`, `lib/segment/src/index/sparse_index/sparse_vector_index/read_view/search.rs:269`.

IDF multiplies query weights by

\[
\ln\left(1+\frac{N-df+0.5}{df+0.5}\right).
\]

Statistics aggregate segments within a shard, an optional corpus filter changes the population used for statistics independently of retrieval filtering. Documentation correctly says per-shard, the source configuration comment calling the default collection-wide is inconsistent with the shard aggregation path. Sources: `lib/segment/src/data_types/query_context.rs:92`, `lib/segment/src/data_types/query_context.rs:353`, `lib/shard/src/query/query_context.rs:75`, `lib/segment/src/index/sparse_index/sparse_vector_index/read_view/idf.rs:51`, `lib/segment/src/types.rs:712`; documentation: `manage-data/indexing.md:426`.

## GPU construction and segment-result merging

The optional GPU backend uses `ash`, `gpu-allocator` and `shaderc`, compiling GLSL to SPIR-V and loading Vulkan 1.3. Construction keeps a sequential CPU bootstrap, processes GPU work by graph level and downloads links back into the builder, effective construction width is at least `m0`. This is graph-building acceleration, the inspected ordinary query path remains CPU-side. Sources: `lib/gpu/Cargo.toml:14`, `lib/gpu/src/instance.rs:98`, `lib/segment/src/index/hnsw_index/gpu/gpu_graph_builder.rs:36`.

GPU upload and graph-build failures have CPU fallback paths, cancellation propagates, some GPU-context creation failures propagate before those fallback branches. Unsupported quantized formats fall back to uploading original representations, Turbo4 storage is dequantized for upload. Sources: `lib/segment/src/index/hnsw_index/hnsw/gpu_build.rs:90`, `lib/segment/src/index/hnsw_index/hnsw/gpu_build.rs:171`, `lib/segment/src/index/hnsw_index/gpu/gpu_vector_storage/mod.rs:101`, `lib/segment/src/index/hnsw_index/gpu/gpu_vector_storage/mod.rs:150`.

Segment search collects handles under the holder lock, releases that lock and launches blocking search tasks, each task holds its segment read guard while searching. Results feed version-aware bounded queues, the aggregator first collects maximum versions among returned candidates, rejects stale returned versions and deduplicates external point IDs. Sources: `lib/collection/src/collection_manager/segments_searcher.rs:224`, `lib/collection/src/collection_manager/segments_searcher.rs:661`, `lib/shard/src/search_result_aggregator.rs:11`, `lib/shard/src/search_result_aggregator.rs:50`.

Multi-segment search can sample smaller per-segment result counts using a Poisson-based allocation bounded by an HNSW ef floor, then rerun selected segment/query pairs at the full requested count when their returned boundary suggests more competitive results. These checks operate on available ANN results, they do not establish exhaustive global recall. Sources: `lib/collection/src/collection_manager/segments_searcher.rs:140`, `lib/collection/src/collection_manager/segments_searcher.rs:303`, `lib/collection/src/collection_manager/segments_searcher.rs:569`.

## Complexity and memory

The following are formulas inferred from the representations and loops above, not measured performance guarantees, let `N` be points, `d` dimensions, `k` requested results, `E` stored directed links and `P` sparse nonzeros.

| Component | Approximate cost or size |
|---|---|
| Original dense storage | `N × d × b` bytes, `b=4/2/1` for float32/float16/uint8 |
| Exact dense scoring | `O(Nd + N log k)`, replace `N` with enumerated candidates when filtering |
| HNSW traversal | `O(Vd + V log ef + links/filter work)`, `V` is actual distance evaluations |
| Uncompressed graph IDs | `4E` bytes plus offsets, layer metadata and entry points |
| MaxSim evaluation | `O(query_rows × document_rows × d)` per point |

HNSW’s hierarchy suggests approximately logarithmic navigation in favorable data, source provides no worst-case logarithmic bound, construction cost depends on construction beams, neighbor diversification, backlinks and data geometry. Main-graph capacity is roughly `2mN + mΣL_i` links before payload additions, payload-field overlap can materially increase `E`. These are inferred from `graph_layers_builder.rs:389`, `graph_layers_builder.rs:514` and `graph_layers_builder.rs:348`.

For one million 768-dimensional vectors, raw float32 values occupy 3.072 GB, scalar codes approximately 772 MB, one-bit codes 96 MB, 1.5-bit codes 144 MB and two-bit codes 192 MB, excluding metadata and alignment effects. At `m=16`, base-layer IDs alone have capacity for approximately 128 MB before compression, so graph memory becomes a major fraction after aggressive vector quantization. These are inferred from the storage formulas, `encoded_vectors_u8.rs:622`, `encoded_vectors_binary.rs:886` and `hnsw_index/config.rs:45`.

PQ uses approximately `N ceil(d/s)` code bytes, `256d` float centroids and query tables of `256 ceil(d/s)` float entries, scoring takes roughly one lookup per subspace after table construction. Sparse memory follows postings count `P`, mutable posting elements contain approximately 12 bytes of fields before container overhead, compressed postings reduce ID storage and can narrow weights. Sources: `lib/quantization/src/encoded_vectors_pq.rs:39`, `lib/sparse/src/index/posting_list.rs:11`, `lib/sparse/src/index/compressed_posting_list.rs:54`.

Visited lists use one generation byte per point, concurrent searches can therefore require approximately `concurrency × N` bytes of visitation state, ACORN uses two lists. Generation wrap periodically clears a list, pooling reduces allocation frequency without removing the concurrency-dependent footprint. Sources: `lib/segment/src/index/visited_pool.rs:19`, `lib/segment/src/index/visited_pool.rs:86`, `lib/segment/src/index/hnsw_index/graph_layers.rs:159`.

# 3. Config knobs and defaults that matter

| Setting | Default/effective behavior | Consequence |
|---|---|---|
| HNSW `m` | 16, base `m0=32` | Controls main-graph connectivity, payload additions are extra |
| `ef_construct` | 100, validated minimum 4 | Construction candidate width, also default search ef |
| Query `hnsw_ef` | Persisted construction ef when absent | Actual beam also accommodates requested/oversampled top |
| Dense `full_scan_threshold` | 10,000 KB | Converted using average original-vector size |
| `payload_m` | Main `m` when absent | Zero disables payload-specific graph construction |

Sources: `lib/segment/src/types.rs:811`, `lib/segment/src/types.rs:1645`, `lib/segment/src/types.rs:2361`, `lib/segment/src/index/hnsw_index/config.rs:45`, `lib/segment/src/index/hnsw_index/hnsw/read_view/search.rs:48`.

| Setting | Default/effective behavior | Consequence |
|---|---|---|
| `max_indexing_threads` | 0 means automatic | CPU heuristic uses up to 8 on smaller hosts, 12 for 49–64 CPUs, 16 above that |
| Graph `memory` | Cached, legacy `on_disk=true` maps to cold | Residency policy differs from mere file persistence |
| `inline_storage` | False | Requires quantization, excludes multivectors |
| ACORN | Disabled, maximum selectivity 0.4 | Applies only to qualifying graph searches |
| Optimizer indexing threshold | 10,000 KB | Controls index creation, independently of query planning |

Sources: `lib/common/common/src/defaults.rs:71`, `lib/segment/src/index/hnsw_index/mod.rs:89`, `lib/segment/src/types.rs:836`, `lib/segment/src/types.rs:624`, `lib/shard/src/optimizers/config.rs:17`.

| Setting | Default/effective behavior | Consequence |
|---|---|---|
| Quantization `ignore` | False | Uses available quantization unless exact search overrides it |
| Oversampling | Effective 1, configured values must be ≥1 | Extra candidates consume scoring and optional rescore I/O |
| Rescore | Encoding-specific when absent | Scalar/PQ false, binary true |
| Scalar quantile | Absent means full range, accepted interval 0.5–1 | Clipping exchanges outlier fidelity for resolution |
| Binary/Turbo depth | Binary one-bit, Turbo four-bit | Their 1.5/2-bit variants use different encodings |

Sources: `lib/segment/src/types.rs:575`, `lib/segment/src/types.rs:939`, `lib/segment/src/vector_storage/quantized/quantized_vectors.rs:146`, `lib/segment/src/vector_storage/quantized/quantized_vectors/accessors.rs:16`, `lib/segment/src/types.rs:1083`.

Sparse scan threshold defaults to 5,000 **vectors**, unlike the dense threshold’s KB, sparse appendable indexes default to `MutableRam`. GPU indexing defaults off, force-half defaults false, groups default to 512 and parallel indexed-device slots default to one, integrated/emulated devices default disallowed. Sources: `lib/segment/src/types.rs:2363`, `lib/segment/src/index/sparse_index/sparse_index_config.rs:18`, `src/settings.rs:213`.

# 4. Edge cases, failure modes and known sharp edges

## Recall and filtering

Payload links exist only for eligible indexed blocks, an arbitrary conjunction can remain disconnected even if its individual fields contributed links. Adding payload indexes after graph construction requires rebuilding to gain those links, ACORN and multiple seeds reduce this risk without making approximate search exhaustive. Sources: `lib/segment/src/index/hnsw_index/hnsw/build/additional_links.rs:37`, `lib/segment/src/index/hnsw_index/graph.rs:219`; documentation: `manage-data/indexing.md:53`, `manage-data/indexing.md:320`.

Graph construction explicitly limits parallelism heuristically and bootstraps sequentially to reduce connectivity problems, excessive threads can damage graph quality according to source comments. Quantization also influences construction neighbors, enabling original-vector rescoring later does not undo those graph choices. Sources: `lib/common/common/src/defaults.rs:71`, `lib/segment/src/index/hnsw_index/hnsw.rs:35`, `lib/segment/src/index/hnsw_index/hnsw/build/main_graph.rs:93`.

## Numerical and scoring semantics

Uint8 is a datatype, it does not automatically perform embedding calibration, float16 rounding and scalar clipping change numerical behavior before search. Near-zero cosine vectors are intentionally left unnormalized, binary sign encoding maps exactly zero to the nonpositive side. Sources: `lib/segment/src/data_types/primitive.rs:76`, `lib/segment/src/data_types/primitive.rs:126`, `lib/segment/src/spaces/tools.rs:14`, `lib/quantization/src/encoded_vectors_binary.rs:559`.

The MaxSim implementation comment claims omitting Euclidean per-pair postprocessing preserves ranking, this is unsafe for sums across multiple query rows. A counterexample is document A with nearest-row distances `(0,3)` and document B with `(2,2)`, sums of distances prefer A, sums of squared distances prefer B, assuming those pairwise minima can be realized in the vector space. This is an inferred mathematical discrepancy with `lib/segment/src/vector_storage/query_scorer/mod.rs:78`, nocturnum should specify squared-distance MaxSim explicitly.

Sparse search does not generally fill results with arbitrary nonoverlapping zero-score points, an empty query returns nothing. The batched indexed path excludes zero accumulated scores, while its single-posting shortcut and candidate-first scoring can retain overlapping points with zero scores, so exactness needs a defined candidate universe and zero-score policy. Sources: `lib/segment/src/index/sparse_index/sparse_vector_index/read_view/search.rs:269`, `lib/sparse/src/index/search_context.rs:92`, `lib/sparse/src/index/search_context.rs:145`, `lib/sparse/src/index/search_context.rs:190`.

## Operational and persistence boundaries

Graph links, graph metadata and configuration are saved separately, link replacement is atomic at the file level, these inspected paths do not establish a single atomic transaction across the entire index. Crash consistency must therefore be understood with the surrounding segment publication/recovery protocol, inferred from `lib/segment/src/index/hnsw_index/graph_layers_builder.rs:213` and `lib/segment/src/index/hnsw_index/hnsw/build.rs:247`.

Cold originals plus rescoring can produce many random reads, inline storage reduces separate reads by duplicating neighbor vectors but increases files with graph degree. Inline optimized search also has a distinct path and is bypassed for ACORN, its performance and parity need separate validation. Sources: `lib/segment/src/index/hnsw_index/graph_links/view.rs:77`, `lib/segment/src/index/hnsw_index/hnsw/read_view/search.rs:66`.

A notable error path in `FilteredScorer::score_points` converts a batched filter-check error into zero surviving candidates through `unwrap_or(0)`, with a TODO to propagate the error. This can hide a read/filter failure as fewer results in that path, nocturnum should propagate typed errors. Source: `lib/segment/src/index/hnsw_index/point_scorer.rs:266`.

GPU fallback coverage is incomplete, context creation can fail before the fallback branches, successful Vulkan initialization does not imply every quantization format has a dedicated shader. The inspected binary shader performs XOR/popcount scoring, multi-bit binary parity deserves direct validation rather than assuming every CPU encoding shares its semantics. Sources: `lib/segment/src/index/hnsw_index/hnsw/gpu_build.rs:90`, `lib/segment/src/index/hnsw_index/gpu/gpu_vector_storage/mod.rs:150`, `lib/segment/src/index/hnsw_index/gpu/shaders/vector_storage_bq.comp:10`.

## Cross-segment and IDF boundaries

Version suppression uses versions of returned candidates, it is not by itself proof that an older point cannot appear when a newer version fails the filter or candidate selection. Correct visibility depends on mutation/shadowing invariants outside the aggregator, inferred from `lib/shard/src/search_result_aggregator.rs:50`.

Per-shard IDF can produce shard-dependent scores for equivalent documents, corpus-filter IDF also adds statistics work beyond retrieval. Compressed sparse weights introduce representation error even though posting traversal is not ANN, these are distinct accuracy dimensions. Sources: `lib/shard/src/query/query_context.rs:75`, `lib/segment/src/index/sparse_index/sparse_vector_index/read_view/idf.rs:51`, `lib/segment/src/index/vector_index_base.rs:110`.

# 5. Features that are Cloud-only, enterprise-only or not in the open-source build

The inspected engine declares Apache-2.0 licensing, dense HNSW, filterable links, ACORN, quantization, multivectors and sparse indexes are present in its source without an observed Cloud entitlement gate in the examined implementations. This is a scoped source finding, not an audit of every distribution. Sources: `Cargo.toml:12`, `lib/segment/src/index/vector_index_base.rs:110`, `lib/segment/src/types.rs:575`.

GPU support is an optional compilation feature and is absent from the default feature set, self-hosted GPU builds are documented. Qdrant Cloud adds managed GPU provisioning and supported hardware/resource configurations, the underlying graph-building algorithm is available in the open-source tree. Sources: `Cargo.toml:20`, `Cargo.toml:38`, `lib/gpu/Cargo.toml:14`; documentation: `ops-configuration/running-with-GPU.md:18`, `ops-configuration/running-with-GPU.md:69`, `tutorials-operations/gpu-accelerated-hnsw-indexing.md:21`.

No examined vector-indexing feature requires an enterprise-only implementation, operational packaging and managed infrastructure should be distinguished from algorithm availability.

# 6. Design implications for nocturnum

## Concepts to retain

1. **Separate visibility, representation and retrieval**, use stable internal IDs, explicit deletion/version state, original storage and replaceable indexes, the Qdrant boundaries support this separation but the result aggregator alone cannot enforce visibility. Design recommendation, inferred from `segment/mod.rs:65`, `vector_index_base.rs:28` and `search_result_aggregator.rs:50`.

2. **Keep exact search as a reference implementation**, every datatype, metric, filter and quantizer needs an exact baseline against which ANN recall and score error can be measured separately. Design recommendation, inferred from `hnsw/read_view/dispatch.rs:44` and `vector_index_search_common.rs:62`.

3. **Treat filtered search as a planner problem**, candidate enumeration, cardinality bounds and graph traversal should be interchangeable plans with observable costs, an inexpensive exact route for selective filters is valuable before payload-aware graph complexity. Design recommendation, inferred from `hnsw/read_view/dispatch.rs:106`.

4. **Persist immutable indexed generations**, construct a complete generation and publish it through one explicit manifest, keep mutable ingestion separate and make cancellation/error handling part of index construction. Design recommendation, inferred from `hnsw/vector_index_impl.rs:79`, `graph_layers_builder.rs:213` and `hnsw/build.rs:247`.

5. **Expose encoding-specific behavior**, quantized storage size, query precision, rescore defaults and graph-construction scoring should be explicit, avoid presenting a single compression ratio as a universal accuracy or performance promise. Design recommendation, inferred from the distinct quantization implementations above.

## Minimal v1 and later phases

| Phase | Proposed cut | Reason |
|---|---|---|
| Minimal v1 | Float32, named spaces, dot/cosine/squared-L2, exact scan, deterministic top-k and filter candidate enumeration | Establish scoring and visibility contracts before approximation |
| First ANN release | One CPU HNSW builder, sequential construction initially, immutable mmap graph, configurable `m/ef_construct/ef` | Keep concurrency and persistence independently understandable |
| First compression release | Scalar quantization, oversampling, original-vector rescoring, measured RAM/I/O behavior | Lowest-complexity encoded scorer with a clear correctness baseline |
| Later retrieval | Payload-aware links, ACORN, sparse postings/IDF and MaxSim reranking | Each adds distinct planning or semantic obligations |
| Later acceleration | Parallel build, graph compression, binary/PQ/TurboQuant, inline vectors and Vulkan construction | Require workload-specific benchmarks and format compatibility tests |

These are design recommendations, inferred from the implementation complexity and interactions documented in sections 2–4.

For multivectors, begin with reranking candidates retrieved through a cheaper dense or sparse space, direct HNSW construction under directional MaxSim multiplies matrix-scoring work and weakens familiar geometric assumptions. Candidate recall then becomes an explicit prerequisite for reranker quality, inferred from `multi_metric_query_scorer.rs:64` and `multi_metric_query_scorer.rs:113`.

For graph construction, establish deterministic seeds and a serial reference before parallel insertion, persist the random-level convention as part of reproducibility expectations. Compression should use a versioned, independently specified layout with checked bounds and dimensions, algorithm compatibility does not require Qdrant file compatibility. Design recommendations, inferred from `graph_layers_builder.rs:389`, `hnsw/build/main_graph.rs:114` and `graph_links/header.rs:25`.

## Candidate Rust crates

These candidates are already used in the inspected tree, their inclusion here is a design inference rather than a current ecosystem comparison.

| Purpose | Candidates | Evidence |
|---|---|---|
| Parallel work and locks | `rayon`, `parking_lot` | `lib/segment/Cargo.toml:55`, `lib/segment/Cargo.toml:56` |
| Mmap and packed postings | `memmap2`, `bitpacking` | `lib/sparse/Cargo.toml:18`, `lib/sparse/Cargo.toml:25` |
| Numeric/storage representations | `half`, `bytemuck`, `zerocopy` | `lib/segment/Cargo.toml:50`, `lib/segment/Cargo.toml:98`, `lib/segment/Cargo.toml:125` |
| Metadata and ordered scores | `serde`, `serde_json`, `ordered-float` | `Cargo.toml:258`, `Cargo.toml:289`, `Cargo.toml:294` |
| Optional Vulkan backend | `ash`, `gpu-allocator`, `shaderc` | `lib/gpu/Cargo.toml:22` |

Use standard-library heaps and scalar kernels for the first implementation, add runtime SIMD dispatch only after numerical parity tests exist, this avoids coupling early correctness to architecture-specific code. Design recommendation, inferred from the separate scalar and SIMD paths in `spaces/simple.rs:129` and `turboquant/simd/mod.rs:49`.

# 7. Open questions

1. **What are nocturnum’s exact scoring contracts?** Decide whether Euclidean output means distance or squared distance, specify zero vectors, ties, finite-value validation, sparse nonoverlap/zero results and MaxSim aggregation, the inspected source exposes materially different edge semantics. Evidence: `spaces/simple.rs:36`, `spaces/tools.rs:14`, `query_scorer/mod.rs:78`, `lib/sparse/src/index/search_context.rs:145`.

2. **Which invariants establish newest-version visibility across segments?** Trace mutation/shadowing and second-pass merge behavior before adopting the aggregator design, returned-candidate version comparisons alone do not establish the full invariant. Evidence: `lib/shard/src/search_result_aggregator.rs:50`, `lib/collection/src/collection_manager/segments_searcher.rs:303`.

3. **How should IDF behave across shards and deletions?** The current operational path is shard-local, global statistics read posting lengths and vector counts, deletion freshness and collection-wide normalization require further lifecycle inspection. Evidence: `lib/shard/src/query/query_context.rs:75`, `lib/segment/src/index/sparse_index/sparse_vector_index/read_view/idf.rs:51`.

4. **Which compressed/GPU combinations preserve CPU scoring and graph quality?** Validate multi-bit binary shaders, asymmetric queries, inline search and fallback paths against exact float32 retrieval, measure quantization score error separately from ANN miss rate. Evidence: `gpu/shaders/vector_storage_bq.comp:10`, `hnsw/gpu_build.rs:90`, `hnsw/read_view/search.rs:66`.

5. **What workload should determine the v1 planner?** Define dimensions, vector population, filter selectivity, update rate and memory budget before choosing thresholds, the immediate specification step is to write one representative query with its filter, expected result count and permitted latency. This is a proposed next action, inferred from `hnsw/read_view/dispatch.rs:106` and `hnsw.rs:189`.