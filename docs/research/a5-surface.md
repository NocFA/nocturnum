# 1. Summary

- **Qdrant exposes two substantial client protocols and a separate peer protocol**, REST uses Actix Web, public gRPC uses Tonic, and internal gRPC carries consensus, replication and shard management (`src/actix/mod.rs:101`, `src/tonic/mod.rs:112`, `src/tonic/mod.rs:240`).
- **Authentication and operational primitives are open source**, including static administrator/read-only keys, JWT collection permissions, TLS, audit logging, Prometheus metrics, issues, snapshots and object-store snapshot destinations (`src/common/auth/mod.rs:115`, `lib/storage/src/rbac/mod.rs:31`, `lib/storage/src/audit.rs:50`, `lib/collection/src/common/snapshots_manager.rs:26`).
- **The production base configuration needs deliberate hardening**, it binds all interfaces, enables CORS, leaves authentication and TLS unset, and leaves peer authentication enforcement disabled (`config/config.yaml:358`, `config/config.yaml:431`, `src/settings.rs:75`).
- **The dashboard is a separate React application served as static files**, with collection administration, point inspection/editing, an API console, ANN recall measurement, snapshots, graph exploration, dimensionality reduction, tutorials, datasets, JWT generation and quotas (`qdrant-web-ui/src/routes.jsx:17`, `qdrant-web-ui/src/pages/Collection.jsx:85`, `src/actix/web_ui.rs:10`).
- **For nocturnum, implement a declared Qdrant compatibility subset**, with faithful REST and gRPC behavior for supported operations, while keeping storage formats, peer protocols and administration independently designed (inferred from the protocol breadth and deprecated encodings in `lib/api/src/grpc/proto/points.proto:87`).

Engine citations are relative to the root of the `qdrant/qdrant` repository, UI citations start with `qdrant-web-ui/`, the root of the `qdrant/qdrant-web-ui` repository, and documentation citations start with `documentation/`, relative to `qdrant-landing/content/` in the `qdrant/landing_page` repository.

The supplied engine identifies itself as **1.19.2**, this report describes that supplied tree, without building, running or modifying it (`Cargo.toml:1`).

# 2. How it works, fundamentally

## REST transport and schema generation

The REST server shares an `Arc<Dispatcher>` through Actix `web::Data`, handlers convert validated transport requests into storage/collection operations, with authentication carried in an `Auth` request context (`src/actix/mod.rs:62`, `src/actix/auth.rs:144`).

Middleware provides response compression, authentication, path normalization, optional permissive CORS, request logging and telemetry, while path/query/body validation distinguishes malformed input from validation failures (`src/actix/mod.rs:101`, `src/actix/mod.rs:229`).

Ordinary JSON responses use `ApiResponse<D>`, containing optional `result`, `status`, elapsed `time` and optional resource `usage`, while the root endpoint returns `VersionInfo` directly (`lib/api/src/rest/models.rs:15`, `lib/api/src/rest/models.rs:33`, `src/actix/mod.rs:57`).

OpenAPI is assembled through a build-time pipeline, rather than generated directly from Actix route annotations.

`schema_generator` gathers Rust types under `AllDefinitions`, emits Draft-07 JSON Schema through `schemars`, and `tools/generate_openapi_models.sh` renders handwritten YTT endpoint templates, converts schemas, merges documents, validates them and copies the result into the documentation tree (`src/schema_generator.rs:42`, `src/schema_generator.rs:98`, `tools/generate_openapi_models.sh:25`).

The main template declares OpenAPI **3.0.1**, so route registration, endpoint templates and model generation form separate maintenance surfaces (inferred; `openapi/openapi-main.ytt.yaml:3`).

## Complete REST endpoint-group inventory

In this table, `C` means `/collections/{collection_name}`, `P` means `C/points`, and `S` means `C/shards/{shard}`.

| Group | Registered operations and paths | Evidence |
|---|---|---|
| Identity | `GET /`, returns title/version/optional commit | `src/actix/mod.rs:57` |
| Collections and aliases | List/get/existence/create/update/delete collections, list global/collection aliases, atomic alias actions through `POST /collections/aliases` | `src/actix/api/collections_api.rs:37` |
| Collection administration | `GET/POST C/cluster`, `GET C/optimizations`, `GET C/memory` | `src/actix/api/collections_api.rs:201`, `:279`, `:303` |
| Point retrieval | `GET P/{id}`, `POST P`, `POST P/scroll`, `POST P/count` | `src/actix/api/retrieve_api.rs:75`, `:132`, `:194`; `src/actix/api/count_api.rs:17` |
| Point/vector mutations | Upsert at `PUT P`, delete at `POST P/delete`, vector update/delete, heterogeneous updates at `POST P/batch` | `src/actix/api/update_api.rs:32`, `:79`, `:112`, `:159`, `:324` |
| Payload and indexes | Set/overwrite/delete/clear payload, create/delete payload-field indexes | `src/actix/api/update_api.rs:192`, `:225`, `:258`, `:291`, `:371`, `:407` |
| Named-vector schema | `PUT/DELETE C/vectors/{vector_name}` | `src/actix/api/vector_name_api.rs:22`, `:57` |
| Legacy similarity search | `P/search`, `/search/batch`, `/search/groups`, all POST | `src/actix/api/search_api.rs:26`, `:90`, `:167` |
| Recommendation/discovery | `P/recommend`, `/recommend/batch`, `/recommend/groups`, `/discover`, `/discover/batch` | `src/actix/api/recommend_api.rs:30`, `:131`, `:189`; `src/actix/api/discover_api.rs:20`, `:85` |
| Universal query and analytics | `P/query`, `/query/batch`, `/query/groups`, `C/facet`, `P/search/matrix/pairs`, `/matrix/offsets` | `src/actix/api/query_api.rs:31`, `:112`, `:203`; `src/actix/api/facet_api.rs:18`; `src/actix/api/search_api.rs:224`, `:282` |
| Shard keys/local shard operations | List/create/delete shard keys, shard-local retrieve/scroll/count and cleanup | `src/actix/api/shards_api.rs:17`, `:34`, `:64`; `src/actix/api/local_shard_api.rs:37`, `:80`, `:158`, `:240` |
| Snapshots | Collection and whole-storage list/create/download/delete, collection upload/recovery, shard list/create/stream/upload/recovery/download/delete, partial snapshot create/recover/recover-from/manifest | `src/actix/api/snapshot_api.rs:152`, `:193`, `:251`, `:309`, `:378`, `:645`, `:768`, `:942` |
| Cluster administration | Cluster status/recovery/peer removal, metadata key CRUD, cluster telemetry | `src/actix/api/cluster_api.rs:49`, `:60`, `:75`, `:113`, `:201` |
| Operations/security | `/telemetry`, `/metrics`, `/issues`, `/healthz`, `/livez`, `/readyz`, `/audit/logs`, `/quotas` | `src/actix/api/service_api.rs:47`, `:92`, `:150`; `src/actix/api/issues_api.rs:7`; `src/actix/api/audit_api.rs:47`; `src/actix/api/quota_api.rs:28` |
| Diagnostics/recovery | `/stacktrace`, GET/POST `/logger`, GET/PATCH `/debugger`, profiler slow-request/consensus-lag reports, `C/truncate_unapplied_wal` | `src/actix/api/service_api.rs:142`, `:185`, `:201`, `:233`; `src/actix/api/debug_api.rs:7`; `src/actix/api/profiler_api.rs:33`, `:59` |

Additional routes are compile-feature dependent, including `C/debug` staging operations and shard WAL/recovery-point inspection under `staging` (`src/actix/api/update_api.rs:434`, `:485`, `src/actix/api/debug_api.rs:32`, `:134`).

Static dashboard files form another HTTP surface at `/dashboard`, rather than a database operation group (`src/actix/web_ui.rs:40`).

## gRPC transport and services

The protocol entry point is `lib/api/src/grpc/proto/qdrant.proto`, with message definitions split into collections, points, common values, snapshots, health and internal-service files.

`tonic_prost_build` compiles these definitions, adds validation attributes and emits a descriptor set used for reflection (`lib/api/build.rs:41`).

The public listener registers the following services:

| Service | RPC inventory |
|---|---|
| `qdrant.Qdrant` | `HealthCheck` |
| `qdrant.Collections` | `Get`, `List`, `Create`, `Update`, `Delete`, `UpdateAliases`, `ListCollectionAliases`, `ListAliases`, `CollectionClusterInfo`, `CollectionExists`, `UpdateCollectionClusterSetup`, `CreateShardKey`, `DeleteShardKey`, `ListShardKeys` |
| `qdrant.Points` | `Upsert`, `Delete`, `Get`, `UpdateVectors`, `DeleteVectors`, `SetPayload`, `OverwritePayload`, `DeletePayload`, `ClearPayload`, `CreateFieldIndex`, `DeleteFieldIndex`, `CreateVectorName`, `DeleteVectorName`, `Search`, `SearchBatch`, `SearchGroups`, `Scroll`, `Recommend`, `RecommendBatch`, `RecommendGroups`, `Discover`, `DiscoverBatch`, `Count`, `UpdateBatch`, `Query`, `QueryBatch`, `QueryGroups`, `Facet`, `SearchMatrixPairs`, `SearchMatrixOffsets` |
| `qdrant.Snapshots` | `Create`, `List`, `Delete`, `CreateFull`, `ListFull`, `DeleteFull` |
| Health/reflection/raw storage | `grpc.health.v1.Health/Check`, reflection v1/v1alpha, and `qdrant.StorageRead`: `ListFiles`, `FileExists`, `FileLength`, `ReadBytes`, `ReadBytesStream`, `ReadWhole`, `ReadBatch` |

Sources: `lib/api/src/grpc/proto/collections_service.proto:8`, `points_service.proto:8`, `snapshots_service.proto:8`, `storage_read_service.proto:21`, and actual registration at `src/tonic/mod.rs:179`.

The internal listener separately mounts `Qdrant`, `QdrantInternal`, `CollectionsInternal`, `PointsInternal`, `ShardSnapshots` and `Raft`, importantly, `ShardSnapshots` being defined in protobuf does **not** mean it is available on public port 6334 (`src/tonic/mod.rs:316`).

Public services support gzip compression and reflection, with explicitly selected public reflection service names (`src/tonic/mod.rs:179`, `:229`).

**Documentation disagreement:** `documentation/interfaces.md:30` says all interaction takes place through REST, and `:39` claims each REST endpoint has a corresponding gRPC method.

Actual registration disproves both claims, particularly for metrics, issues, audit, quotas, uploads/downloads and diagnostics (`src/actix/mod.rs:151`, `src/tonic/mod.rs:179`).

## Authentication, authorization and TLS

`AuthKeys` holds primary/alternate administrator keys, a read-only key and JWT decoders.

No nonempty configured key means no authentication middleware, with handlers falling back to full access, while `api-key` takes precedence over the exact `Authorization: Bearer ` prefix (`src/common/auth/mod.rs:115`, `:241`, `src/actix/auth.rs:174`).

Static administrator keys produce global manage access, read-only keys produce global read access, and comparison uses `ct_eq` (`src/common/auth/mod.rs:151`, `:292`).

JWT RBAC uses **HS256**, signed with the administrator or alternate key.

Expiration is optional, audience validation is disabled, expiration leeway is 30 seconds, and absent `access` defaults to full access (`src/common/auth/jwt_parser.rs:15`, `src/common/auth/claims.rs:8`, `:48`).

`Access` is either global `"r"`/`"m"`, or a collection list with `"r"`, `"rw"` and `"prw"` permissions.

`"prw"` permits point changes while excluding collection extras such as snapshots and payload indexes, with `AccessRequirements`, `CollectionPass` and `CollectionMultipass` carrying checked access into storage operations (`lib/storage/src/rbac/mod.rs:31`, `:84`, `:233`, `:262`).

Claims additionally support `value_exists`, which performs a filtered collection lookup on each validation, plus `sub` for inference identity and `subject` for audit identity (`src/common/auth/claims.rs:8`, `src/common/auth/mod.rs:176`, `:246`).

Payload-constrained JWT collection claims were removed, the retained `payload` field explicitly fails validation so old constrained tokens cannot silently gain wider access (`lib/storage/src/rbac/mod.rs:54`).

REST excludes root, health/readiness/liveness and available dashboard assets from key authentication.

gRPC excludes both health-check method paths, while other methods use a shared Tower authentication layer (`src/actix/mod.rs:91`, `src/tonic/auth.rs:67`).

Internal authentication enforcement is opt-in and accepts only administrator keys, rejecting narrowed JWTs and read-only keys (`src/tonic/mod.rs:263`, `src/common/auth/mod.rs:213`).

HTTPS uses Rustls and supports optional client-certificate verification plus certificate refresh on TTL expiration.

Public gRPC loads server identity only, internal gRPC additionally verifies client certificates against the configured CA, and gRPC certificates do not receive the HTTPS hot-reload behavior (`src/actix/certificate_helpers.rs:32`, `src/common/helpers.rs:22`, `config/config.yaml:517`).

## Telemetry, metrics, health and issues

`TelemetryCollector` combines application/build details, collection state, cluster state, requests, allocator memory, hardware counters, search-pool state and resource quotas.

Collection telemetry uses blocking workers because it acquires synchronous locks, with a default preparation timeout of 60 seconds (`src/common/telemetry.rs:37`, `:49`, `:107`).

`GET /telemetry` defaults to detail level zero, non-anonymized output and no per-collection expansion.

`GET /metrics` requires global read access, requests richer telemetry with histograms, and renders Prometheus text (`src/actix/api/service_api.rs:47`, `:92`).

Metric families cover build/status, collection counts/vectors/replicas/optimizations/update queues, REST/gRPC responses and latency, cluster consensus, allocator memory, hardware I/O, quotas and Linux process information (`src/common/metrics/app.rs:10`, `collections.rs:258`, `requests.rs:360`, `cluster.rs:21`, `memory.rs:10`, `procfs_metrics.rs:62`).

An optional dedicated metrics listener uses one Actix worker and bypasses API-key middleware, with the same configured bind host and optional TLS (`src/actix/metrics_service.rs:12`).

Outbound usage reporting posts anonymized level-two telemetry immediately and subsequently every hour to the production or staging telemetry service (`src/common/telemetry_reporting.rs:14`, `:37`, `:75`).

`/healthz` and `/livez` simply return success when their handlers execute.

`/readyz` uses `HealthChecker` in clustered deployments, with an atomic readiness latch, notifications and a 500 ms wait, once true it remains true (`src/actix/api/service_api.rs:150`, `src/common/health.rs:23`, `:71`).

The standard gRPC health service always answers `Serving`, so it also does not represent current shard readiness (`src/tonic/mod.rs:77`).

Issues are an in-process `DashMap<Code, IssueRecord>`, keyed by issue type and instance.

Reads filter issues by authorized collections, clearing requires global manage access, and issue clearing is separate from resolving the underlying condition (`lib/common/issues/src/dashboard.rs:9`, `:38`, `src/actix/api/issues_api.rs:7`).

## Snapshots, deployment and lifecycle

Collection snapshots are `.snapshot` tar archives containing shard data, collection version/configuration, shard-key mapping and payload-index schema.

The creation path gathers shard snapshots, finishes a temporary archive and hands it to `SnapshotStorageManager`, listener nodes can omit WAL because they serve a dedicated backup role (`lib/collection/src/collection/snapshots.rs:65`, `:100`, `:128`).

Collection snapshots exclude aliases, while whole-storage snapshots collect collection snapshots plus alias mappings (`documentation/snapshots.md:20`, `lib/storage/src/content_manager/snapshots/mod.rs:116`).

The OSS snapshot manager supports local storage, S3-compatible storage, GCS and Azure Blob Storage, with explicit credentials or provider credential chains (`lib/collection/src/common/snapshots_manager.rs:26`).

Recovery supports uploads, local file URLs and remote URLs, optional SHA-256 verification, and reconciliation priorities `replica`, `snapshot`, `no_sync`, plus internal shard-transfer handling (`src/actix/api/snapshot_api.rs:193`, `:251`, `lib/collection/src/operations/snapshot_ops.rs:17`).

Whole-collection recovery requires global manage permission, and is spawned separately to protect its non-cancel-safe inner operation (`lib/storage/src/content_manager/snapshots/recover.rs:60`).

Startup creates resource budgets, peer channels, `TableOfContent`, collections and shared runtimes before launching the REST, optional metrics and optional gRPC threads.

An initialized-file indicator is written after launching service threads (`src/main.rs:522`, `:554`, `:738`, `:819`).

Docker packages the binary, configuration, dashboard, entrypoint and SBOM, sets `RUN_MODE=production`, and defaults its configurable UID to **0** (`Dockerfile:132`, `:214`, `:223`, `:233`).

The entrypoint forwards termination signals and optionally starts recovery mode after exit 137 during initialization, controlled by `QDRANT_ALLOW_RECOVERY_MODE` and the initialized-file marker (`tools/entrypoint.sh:9`, `:29`).

Binary CLI controls include bootstrap/peer URI, alternate config path, telemetry disablement, collection/full-storage startup recovery, forced snapshot replacement and consensus reinitialization (`src/main.rs:75`).

## Web UI and ecosystem

The UI package is **0.2.19**, using React 19, React Router, Vite, Material UI/Emotion, Monaco, Axios, the Qdrant REST client, MDX tutorials, Uppy uploads and graph-layout WASM (`qdrant-web-ui/package.json:1`).

Every routed page is visible in `src/routes.jsx`:

| Page family | Features |
|---|---|
| Home/welcome | Landing and onboarding pages |
| Collections/create/detail | Collection browsing/creation, point browser, configuration/info, aliases/index management, optimizer/memory/cluster views, ANN Recall and snapshots |
| Console | Editable method/path plus JSONC requests, execution/results, OpenAPI autocomplete, command snippets, saved code and history |
| Visualize/graph | Dimensionality reduction and interactive similarity exploration |
| Datasets/tutorials/JWT/settings | Dataset imports, executable learning material, token generation and quota administration |

Evidence: `qdrant-web-ui/src/routes.jsx:17`, `src/pages/Collection.jsx:85`, `src/components/CodeEditorWindow/config/RequesFromCode.js:6`, `src/pages/Settings.jsx:7`.

The points browser supports scrolling, ID/payload/full-text filters, named-vector similarity queries, facet-backed suggestions, payload/vector inspection, point deletion, payload merge/replacement and index creation/deletion (`qdrant-web-ui/src/components/Points/PointsTabs.jsx:56`, `:82`, `:110`, `PayloadEditor.jsx:19`, `PayloadIndexDialog.jsx:237`).

The ANN tool queries sampled point IDs twice, once with `exact:true` and once with chosen approximate parameters, excludes the source point and computes ID-set overlap divided by exact-result count.

This measures ANN recall, with elapsed search times, rather than relevance judged by people (`qdrant-web-ui/src/components/Collections/SearchQuality/check-index-precision.js:14`).

Visualize supports UMAP, t-SNE and PCA, named vectors, filtering, payload/query-based coloring, selection and highlighting.

UMAP/t-SNE consume server search-matrix output, PCA retrieves raw vectors, and WASM layouts run in a worker (`qdrant-web-ui/src/components/VisualizeChart/requestData.js:4`, `:18`, `:111`, `worker.js:3`, `src/pages/Visualize.jsx:51`).

Graph exploration starts from a point or sampled matrix pairs, retrieves payloads, expands neighbors through query-by-ID and can derive a spanning tree, it presents similarity relationships rather than the internal HNSW graph (inferred; `qdrant-web-ui/src/lib/graph-visualization-helpers.js:1`, `:42`, `:80`, `:98`).

Snapshots can be listed, created, downloaded with progress/cancellation, deleted and uploaded for recovery (`qdrant-web-ui/src/components/Snapshots/SnapshotsTab.jsx:59`, `SnapshotUploadForm.jsx:80`).

Tutorials cover quickstart, content loading, beginner/advanced/full-text filtering, multivectors, sparse vectors, hybrid search and multitenancy (`qdrant-web-ui/src/components/InteractiveTutorial/TutorialSubpages.jsx:17`).

Datasets fetch an external snapshot manifest and filter entries by server version (`qdrant-web-ui/src/pages/Datasets.jsx:25`).

JWTs are signed in-browser with `jose`, while connection/API-key settings persist in local storage (`qdrant-web-ui/src/pages/Jwt.jsx:14`, `src/context/client-context.jsx:12`).

The server serves files from `./static` by default, adds `X-Frame-Options: DENY`, and requires the directory to exist.

Docker obtains the dashboard through `sync-web-ui.sh`, which downloads the latest UI release and replaces its OpenAPI file with the engine specification (`src/actix/web_ui.rs:10`, `:40`, `tools/sync-web-ui.sh:10`).

Official clients cover Python, JavaScript/TypeScript, Rust, Go, .NET and Java, with generated-client possibilities through OpenAPI/protobuf (`documentation/interfaces.md:13`).

FastEmbed is a separate ONNX-based embedding ecosystem, supporting dense, sparse, multivector and reranking workflows, the Python client exposes a FastEmbed extra (`documentation/fastembed/_index.md:18`, `:28`, `documentation/interfaces.md:21`).

Server inference accepts documents/images/objects, partitions local versus remote model inputs, runs local BM25 on blocking workers and sends remote inputs to a configured HTTP inference service (`src/common/inference/service.rs:53`, `:137`, `:193`).

Local names `bm25` and `qdrant/bm25` work without a remote address, with distinct document/query embedding paths (`src/common/inference/local_model.rs:12`, `:60`, `service.rs:379`).

## Qdrant Edge

`lib/edge` is an Apache-2.0 in-process engine library, exposing `EdgeShard`, `ReadOnlyEdgeShard`, `UpdateOnlyEdgeShard`, read traits and typed request builders (`lib/edge/Cargo.toml:1`, `lib/edge/src/lib.rs:16`).

`EdgeShard` owns persisted configuration, a mutex-protected `SerdeWal`, a `LockedSegmentHolder`, optional segment manifest and a Rayon search pool.

Writes append to WAL before acquiring the update lock and applying segment changes, configuration persists to `edge_config.json` (`lib/edge/src/edge_shard/mod.rs:33`, `:49`, `update.rs:28`).

Search/query/scroll/retrieve/count/facet are in-process operations, optimization is explicitly blocking, and snapshots support restoration and partial merging (`lib/edge/src/edge_shard/shard_read.rs:48`, `optimize.rs:26`, `snapshots.rs:11`).

Python and Rust bindings are documented, with pre-provisioned FastEmbed models enabling offline embedding/search, Edge is a library deployment model rather than another REST/gRPC server (`documentation/edge/_index.md:29`, `documentation/edge/edge-fastembed-embeddings.md:17`, `documentation/edge/edge-vs-qdrant-cluster.md:43`).

# 3. Config knobs and defaults that matter

## Configuration precedence

`Settings::new` merges embedded defaults, `config/config`, `config/$RUN_MODE`, `config/local`, Debian package configuration when compiled with `deb`, explicit `--config-path`, and finally environment variables.

`RUN_MODE` defaults to `development`, nested variables use double underscores, for example `QDRANT__SERVICE__API_KEY` (`src/settings.rs:293`).

The development overlay changes binding to loopback, log level to DEBUG, search threads to four, target segments to two, enables all feature flags and audit logging, and tolerates collection-load errors.

Docker explicitly selects production, so a bare binary’s effective defaults depend on overlays and working directory (`config/development.yaml:8`, `Dockerfile:233`).

| Area | Important defaults/settings |
|---|---|
| HTTP/gRPC | Base host `0.0.0.0`, HTTP 6333, public gRPC 6334, JSON request limit 32 MiB, workers zero means available CPUs minus one, minimum one |
| HTTP timeouts | Keepalive, client-request and disconnect timeouts default to five seconds |
| Exposure/auth | CORS true, TLS false, HTTPS client-certificate verification false, keys absent, JWT RBAC false, internal auth enforcement false |
| Dashboard/metrics | Static content enabled, directory `./static`, separate metrics port absent, configurable metrics prefix, slow-query threshold and hardware reporting |
| Snapshot security | Remote URL recovery true by default |
| TLS | Certificate/key/CA paths under `./tls`, HTTPS certificate TTL 3600 seconds |
| Storage paths | `./storage`, `./snapshots`, temporary path null selects storage snapshot temporary directory |
| WAL | 32 MiB segments, zero preallocated segments, one closed segment retained |

Sources: `config/config.yaml:17`, `:106`, `:358`, `:498`, `src/settings.rs:26`, `src/actix/web_ui.rs:10`.

Commenting out `grpc_port` in an overlay does not disable it because the embedded base supplies 6334, an explicit null is required (`config/config.yaml:385`).

Storage/performance configuration additionally includes update concurrency/queue/rate limits, search timeout, search threads, optimizer CPU/I/O budgets, optimization runtime threads, incoming/outgoing automatic shard-transfer limits, async scoring, io_uring mode, load concurrency, mmap advice, recovery mode and collection-load error handling (`lib/storage/src/types.rs:39`, `:80`).

Load defaults are one collection, one shard and eight segments concurrently, automatic incoming/outgoing transfers default to one each (`lib/common/common/src/load_concurrency.rs:8`, `lib/storage/src/types.rs:74`).

| Index/collection setting | Base default |
|---|---|
| Optimizers | Deleted fraction 0.2, vacuum minimum 1000 vectors, automatic target segment count/maximum size, indexing threshold 10,000 KiB, flush interval five seconds, optimization-thread limit null |
| HNSW | `m=16`, `ef_construct=100`, full-scan threshold 10,000 KiB, indexing threads automatic, legacy `on_disk=false`, payload-specific M absent |
| Collection durability | Replication factor one, write consistency factor one, quantization absent |
| Collection limits | Maximum collections absent, strict mode absent, default shard count follows peer count unless explicitly configured |
| Memory/recovery | Payload legacy on-disk true, newer memory-placement settings override legacy booleans, low-memory loading mode disabled |

Sources: `config/config.yaml:74`, `:174`, `:228`, `:267`, `:317`, `lib/segment/src/data_types/collection_defaults.rs:31`.

Strict mode exposes substantially more than the example YAML, including query/time limits, indexed-filter requirements, exact-search permission, HNSW/oversampling caps, upsert/search batch sizes, vector/payload/point budgets, read/write rates, filter complexity, multivector/sparse constraints and payload-index counts (`lib/segment/src/types.rs:1325`).

Node-wide quotas seed from configuration only on first start, subsequently persist in `quota.json` and change through consensus/API.

They can cap resident memory and disk usage, with a default five-percentage-point release margin to avoid write-admission oscillation (`config/config.yaml:321`).

Cluster mode defaults off, P2P uses port 6335 and no TLS, with optional separate bind host.

Consensus defaults include 100 ms ticks, 128-entry WAL compaction, queue size 100, bootstrap timeout 15 seconds and message timeout ten ticks, peer-channel defaults are 60-second RPC timeout, two-second connect timeout and pool size two (`config/config.yaml:461`, `src/settings.rs:573`, `lib/api/src/grpc/transport_channel_pool.rs:25`).

Audit defaults are disabled, directory `./storage/audit`, daily rotation, seven retained files, forwarded-header trust false and API-path logging true (`lib/storage/src/audit.rs:50`).

Additional operator settings include structured/file logging, inference address/token/timeout, GPU device selection/indexing concurrency, Pyroscope diagnostics and feature flags.

Inference’s fallback HTTP timeout is ten minutes, feature flags cover incremental HNSW, appendable quantization, storage layouts, async payload/HNSW, manifests and serverless-compatible storage behavior (`config/config.yaml:3`, `src/settings.rs:230`, `src/common/inference/config.rs:3`, `service.rs:82`, `src/common/debugger.rs:11`, `lib/common/common/src/flags.rs:9`).

# 4. Edge cases, failure modes and known sharp edges

## Security and request admission

**Unset or empty keys disable authentication**, enabling JWT RBAC alone does not create an authentication scheme.

A JWT without `access` receives manage access, without `exp` it has no required expiry, and sharing the administrator key also shares token-minting authority (`src/common/auth/mod.rs:115`, `src/common/auth/claims.rs:48`, `src/common/auth/jwt_parser.rs:28`).

Collection write access is insufficient for whole-collection snapshot recovery, the implementation requires global manage access.

This distinction matters for tenant tooling and client-visible authorization errors (`lib/storage/src/content_manager/snapshots/recover.rs:70`).

JSON size protection is transport-specific, multipart’s overall limit is `usize::MAX`, and gRPC services likewise set decoding limits to `usize::MAX` (`src/actix/mod.rs:110`, `:147`, `src/tonic/mod.rs:183`).

Snapshot upload checks manage access **before** spooling multipart bodies, with source comments naming the fixed read-only-key disk-exhaustion issue GHSA-3v92-w72v-j994.

Authorized uploads still need external or filesystem capacity limits (inferred; `src/actix/auth.rs:217`, `src/actix/api/snapshot_api.rs:193`).

Remote snapshot recovery is an SSRF-capable feature by design, the provided mitigation is disabling remote URL recovery.

Archive extraction rejects links and special entries, disables overwriting and uses `unpack_in` containment (`config/config.yaml:454`, `lib/common/common/src/tar_unpack.rs:19`).

`StorageRead` is publicly mounted despite being documented in its proto as internal/unstable, and its access check requires collection read permission.

It exposes raw shard-file access, with canonicalization and symlink checks intended to constrain paths, this is a larger surface than ordinary point retrieval (`src/tonic/mod.rs:213`, `lib/api/src/grpc/proto/storage_read_service.proto:9`, `src/tonic/api/storage_read_api/helpers.rs:41`, `:94`).

Dashboard keys persist in browser local storage, and UI JWT decoding only controls presentation.

The restricted-route heuristic recognizes collection `"prw"` claims, server-side authorization remains authoritative (`qdrant-web-ui/src/context/client-context.jsx:12`, `src/config/restricted-routes.js:15`).

## Operational interpretation and failure handling

Readiness is a **startup latch**, it cannot report later degradation after becoming true.

If consensus-commit sampling fails, the readiness task can treat synchronization as complete, and only the initially observed unhealthy-shard set is tracked until cleared (`src/common/health.rs:157`, `:177`, `:186`).

Dedicated metrics are unauthenticated, and collection-rich metrics require work and synchronous locks, so scraping configuration affects both exposure and runtime cost (`src/actix/metrics_service.rs:21`, `src/common/telemetry.rs:107`).

**Documentation/source disagreement:** the CLI comment says `--disable-telemetry` disables telemetry collection, actual startup computes an outbound-reporting flag, while local telemetry and metrics remain available (`src/main.rs:120`, `:396`, `:707`, `:743`).

Snapshot `wait=false` returns acceptance while background work continues, failures are subsequently logged, so acceptance does not prove a completed backup (`src/actix/helpers.rs:149`).

Snapshots are node-local, distributed collection recovery needs appropriate snapshots from participating nodes.

Whole-storage snapshots are unsuitable for distributed recovery, collection snapshots exclude aliases, and default `replica` recovery can supersede restored data through synchronization (`documentation/snapshots.md:16`, `:109`, `:135`).

The documented restore compatibility window is the same minor version at an equal/newer patch or the next minor version, it is not an unrestricted portable interchange format (`documentation/snapshots.md:54`).

Temporary snapshot space adds to retained backup storage, while shard recovery serializes through a recovery lock, held without retaining the whole collection’s shard-holder lock (`lib/collection/src/collection/snapshots.rs:71`, `:389`).

OOM recovery detects exit 137 and marker absence, source explicitly notes it cannot distinguish OOM from another kill.

Low-memory startup modes alter loading behavior without changing persisted collection configuration (`tools/entrypoint.sh:36`, `config/config.yaml:89`).

## Protocol and dashboard sharp edges

Error compatibility includes HTTP codes and `Retry-After`, malformed input is 400, validation failure 422, throttling 429, shard unavailability 503 and insufficient storage 507.

Some precondition failures map to 500, and authentication middleware can return plain text rather than the JSON envelope (`src/actix/mod.rs:267`, `src/actix/helpers.rs:206`, `:244`, `src/actix/auth.rs:160`).

Deprecated protobuf vector fields remain alongside newer oneofs, clients using older encodings need conversion compatibility (`lib/api/src/grpc/proto/points.proto:87`).

The ANN recall helper divides by exact-result count without an empty-result guard, and returns null after query exceptions, therefore empty/filter-restricted samples and skipped queries need careful interpretation (`qdrant-web-ui/src/components/Collections/SearchQuality/check-index-precision.js:48`).

Graph tree sorting contains a TODO about dependence on distance metric, so tree appearance should not be treated as a metric-independent structural truth (`qdrant-web-ui/src/lib/graph-visualization-helpers.js:17`).

The dashboard build downloads the **latest** UI rather than a fixed UI version, reproducible packaging needs an explicit version/hash pin (inferred; `tools/sync-web-ui.sh:10`).

# 5. Features that are Cloud-only, enterprise-only or absent from the OSS build

The engine and dashboard are Apache-2.0, and Edge’s package also declares Apache-2.0 (`Cargo.toml:12`, `LICENSE:1`, `qdrant-web-ui/package.json:4`, `lib/edge/Cargo.toml:5`).

The engine feature list contains diagnostics, GPU, packaging and staging switches, with no enterprise-license feature gate in that list (`Cargo.toml:20`).

No CLA policy was established from the inspected repository material, contribution requirements remain an open question rather than an assumed absence.

| Offering | Boundary supported by supplied material |
|---|---|
| OSS database | JWT database permissions, audit logs, metrics, snapshots, object-store snapshot destinations, clustering, dashboard and BM25 are present in source |
| Managed Cloud | Managed provisioning/scaling/upgrades, Cloud Console metrics/logs/email alerts, scheduled backups/retention/restore and hosted inference |
| Cloud RBAC | Account/control-plane permissions for billing, IAM, clusters, Hybrid Cloud and account configuration, separate from database JWT permissions |
| Hybrid Cloud | Enterprise offering using Kubernetes Operator, Cloud Agent and Prometheus Agent in customer infrastructure, outbound control/telemetry connection to Cloud |
| Private Cloud | Commercial Operator without Cloud Console connection, local Kubernetes management and backup CRDs, with customer-managed monitoring/security |

Sources: `documentation/cloud/cluster-monitoring.md:14`, `documentation/cloud/backups.md:32`, `documentation/cloud-rbac/_index.md:13`, `documentation/hybrid-cloud/_index.md:19`, `:52`, `documentation/private-cloud/_index.md:11`.

Managed backup scheduling, retention and infrastructure-level recovery are distinct from the OSS snapshot API.

Private Cloud exposes `QdrantClusterSnapshot`, `QdrantClusterScheduledSnapshot` and `QdrantClusterRestore` resources, the database route inventory does not provide that scheduler/control plane (`documentation/private-cloud/backups.md:10`, `:25`, `:43`).

Private Cloud documentation explicitly excludes central Cloud management UI/API, integrated monitoring/alerting and configuration recommendations, despite an inconsistent monitoring-page reference to the Cloud Console.

The Private Cloud architecture description is the clearer boundary (`documentation/private-cloud/_index.md:25`, `documentation/private-cloud/logging-monitoring.md:56`).

Cloud-hosted embeddings and proxy integrations for OpenAI, Cohere, Jina AI and OpenRouter are documented managed capabilities.

The OSS engine contains generic inference-request plumbing and configurable forwarding, but the supplied trees do not provide the managed inference service itself (`documentation/cloud/inference.md:12`, `:65`, `src/common/inference/service.rs:193`).

Serverless-compatible storage flags and remote storage-read machinery exist in the OSS source, their presence does not establish that the complete managed serverless platform is included (`lib/common/common/src/flags.rs:95`, `lib/api/src/grpc/proto/storage_read_service.proto:17`).

# 6. Design implications for nocturnum

## What to copy conceptually

Use one transport-independent operation layer, with REST and gRPC adapters converting into owned domain requests and a common error model.

Qdrant’s shared `Dispatcher`/`TableOfContent` boundary and shared authentication types demonstrate the value, without requiring nocturnum to reproduce their implementation (inferred; `src/actix/mod.rs:62`, `src/tonic/mod.rs:112`).

Carry checked collection access as an explicit capability, following the conceptual role of `CollectionPass`, and perform authorization before consuming expensive bodies.

Keep point mutations, index administration, snapshots and global management as separate permissions (inferred; `lib/storage/src/rbac/mod.rs:233`, `src/actix/auth.rs:217`).

Expose liveness, startup readiness, current serving readiness and degraded-resource status separately.

Publish useful metrics independently of outbound usage reporting, with outbound reporting opt-in and diagnostic collection bounded (recommendation inferred from `src/common/health.rs:27`, `src/common/telemetry_reporting.rs:75`).

Make backup manifests explicit about collection configuration, aliases, format version, shard coverage and recovery policy.

Provide completed/failed operation status for asynchronous backups and restores (recommendation inferred from `lib/collection/src/collection/snapshots.rs:128`, `src/actix/helpers.rs:149`).

Preserve a small in-process core API so embedded deployment can emerge later, Edge shows that server transports and collection orchestration need not be compulsory dependencies of local vector search (`lib/edge/src/lib.rs:16`, `lib/edge/Cargo.toml:14`).

## What to simplify or change

Prefer a single schema workflow that checks route/schema agreement, Qdrant’s separate handwritten endpoint templates and generated models require additional synchronization (inferred; `tools/generate_openapi_models.sh:25`).

Choose predictable startup configuration, report effective settings and fail on required configuration errors.

Avoid surprising development overlays, ambiguous null/zero semantics and first-start-only settings unless clearly named (recommendation inferred from `src/settings.rs:293`, `config/config.yaml:333`).

Default to loopback or require an explicit public bind, bound JSON/multipart/gRPC requests consistently, and keep raw filesystem access outside the normal public query listener.

Use distinct peer credentials with enforcement enabled when clustering is introduced (recommendation inferred from `src/tonic/mod.rs:263`, `src/tonic/mod.rs:213`).

Keep inference separate from storage durability, allow client-generated vectors first, and add a documented provider interface later.

ANN recall measurement deserves an explicit benchmark interface with empty samples, failures, timings and successful sample counts reported (recommendation inferred from `src/common/inference/service.rs:137`, `qdrant-web-ui/src/components/Collections/SearchQuality/check-index-precision.js:48`).

## Minimal v1 versus later phases

| Phase | External/operational scope |
|---|---|
| Minimal v1 | Single-node service, collection CRUD, point upsert/delete/retrieve/scroll/count, payload mutation/filtering, dense query, static administrator/read-only keys, bounded requests, metrics, logs, truthful health endpoints, local backup/restore, CLI and container |
| Compatibility completion | UUID/u64 IDs, named vectors, payload selectors, exact/approximate search options, batch operations, aliases, payload indexes, legacy search bridge, matching errors and version-handshake behavior |
| Later operations | Collection JWT permissions, audit logs, object-store backups, asynchronous operation tracking, stricter quotas, recall tool and richer dashboard |
| Later search/ecosystem | Sparse/multivector/hybrid query surface, graph/matrix exploration, BM25 and inference integrations, embedded library |
| Separate distributed phase | Replication/consistency guarantees, shard administration, peer security, cluster backup semantics and operator automation |

This is a proposed cut (inferred), the Qdrant routes and proto services establish the dependency and semantic breadth (`src/actix/mod.rs:151`, `lib/api/src/grpc/proto/points_service.proto:8`).

## Candidate Rust crates

The supplied source provides concrete candidate dependencies, versions here describe this checkout rather than a recommendation to use those exact releases:

| Purpose | Candidates |
|---|---|
| Transport/runtime | `actix-web`, `tonic`, `prost`/`tonic-prost-build`, `tower`, `tokio`, `tokio-util` |
| Schemas/configuration | `serde`, `serde_json`, `schemars`, `validator`, `config`, `clap` |
| Security/HTTP | `rustls`, `rustls-pemfile`, `jsonwebtoken`, `reqwest` |
| Operations/storage integration | `prometheus`, `tracing`, `tracing-subscriber`, `tar`, `tempfile`, `object_store` |
| Embedded concurrency | `parking_lot`, `rayon`, `uuid` |

Evidence: `Cargo.toml:65`, `:80`, `:89`, `:96`, `:104`, `:138`, `:203`, `lib/api/build.rs:42`, `lib/edge/Cargo.toml:22`, `lib/common/common/src/tar_unpack.rs:7`.

## Concrete API compatibility recommendation

**Choose a documented Qdrant wire-compatible subset, implemented on both REST and gRPC, with nocturnum’s own administration API.**

A new server can accept existing Qdrant clients for supported operations if it matches service/method names, protobuf field numbers/types/oneofs/enums, optional-field presence, deprecated vector representations, authentication metadata, REST methods/paths/JSON shapes, response envelopes and error behavior.

Point IDs must preserve full unsigned-64-bit values and UUIDs, protobuf defines both explicitly, while the UI already uses special JSON handling for large integers (`lib/api/src/grpc/proto/qdrant_common.proto:9`, `qdrant-web-ui/src/components/CodeEditorWindow/config/RequesFromCode.js:4`).

Semantic compatibility also requires correct filters, payload selection, named-vector resolution, score/distance interpretation, pagination, exact-search options, update completion and consistency promises.

Matching message schemas alone cannot make unsupported semantics compatible (inferred from `lib/api/src/grpc/proto/qdrant_common.proto:23`, `points.proto:11`, `:37`).

Publish a compatibility matrix pinned to tested client versions, validate existing Python/JS/Rust client workflows first and add the remaining official clients as transport coverage expands.

The supplied material proves the protocol definitions and UI’s use of the JS client, it does not establish every external client’s startup/version-check behavior (`documentation/interfaces.md:19`, `qdrant-web-ui/src/common/client.js:9`).

Keep binary snapshots, internal Raft/shard protocols, raw `StorageRead`, managed Cloud APIs and unsupported query families outside the compatibility promise.

Unsupported operations should fail explicitly, while nocturnum identifies itself truthfully and maintains an independently versioned native administration API.

# 7. Open questions

1. **Which client versions and workflows define acceptance?** Obtain client source/tests for startup compatibility checks, retries, deprecated encodings, async writes and protobuf optional-field behavior, the supplied trees enumerate clients but do not contain their full implementations (`documentation/interfaces.md:19`).
2. **How much universal-query behavior belongs in v1?** Existing clients/UI increasingly use query-by-ID and universal queries, a search-only implementation would need a clearly limited compatibility promise (`lib/api/src/grpc/proto/points_service.proto:120`, `qdrant-web-ui/src/components/Points/PointsTabs.jsx:139`).
3. **What is nocturnum’s backup and restore contract?** Define atomicity, format migration, alias inclusion, cancellation behavior and distributed coverage before promising backup portability, Qdrant’s snapshots are physical and node-local (`documentation/snapshots.md:16`, `lib/collection/src/collection/snapshots.rs:128`).
4. **Which supplied-tree additions belong to the intended compatibility baseline?** The tree identifies as 1.19.2 and includes quotas, raw storage reads, extended Edge modes and serverless storage flags, pin source revisions and selected protocol artifacts before synthesis (`Cargo.toml:3`, `src/actix/api/quota_api.rs:28`, `lib/edge/src/lib.rs:25`, `lib/common/common/src/flags.rs:95`).
5. **What contribution/licensing policy should nocturnum adopt?** Qdrant engine/UI/Edge licenses are established, but CLA requirements and separate commercial-component licenses need additional primary material, keep nocturnum implementation independent and its contribution policy explicit (`LICENSE:1`, `qdrant-web-ui/LICENSE:1`, `lib/edge/Cargo.toml:5`).