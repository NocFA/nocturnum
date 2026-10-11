# nocturnum

nocturnum is a vector database written in Rust, for storing embeddings with JSON payloads and searching them by similarity with filters, built to run on one small box at home.

It follows the shape of [Qdrant](https://qdrant.tech/) (collections, points, payload filters, HNSW, a write-ahead log and segments), written from scratch, with everything a self-hoster needs in the open project and the web console built into the same binary.

**Status:** the engine and the REST server run, with crash-tested storage (a write-ahead log, a memtable and immutable segments under a manifest, merged and vacuumed in the background, with snapshots), exact and HNSW vector search, payload indexes for filters, API keys and Prometheus metrics, while the web console and gRPC come next.

The filter semantics are defined in `crates/nocturnum-core/src/filter.rs`, the WAL's on-disk format in `crates/nocturnum-storage/src/wal.rs`, and what each write acknowledgement promises, with the crash tests behind it, in [`docs/storage/durability.md`](docs/storage/durability.md).

The REST API is specified in [`docs/api/v1.md`](docs/api/v1.md), and running and configuring the server in [`docs/server/config.md`](docs/server/config.md).

## Why

Qdrant's engine is open source, but the things that make it pleasant to run for real, scheduled backups, monitoring, automatic rebalancing and the managed console, live in its cloud.

nocturnum aims to be the whole thing in one place, sized for homelab hardware first.

## Layout

| Crate | What it does |
|---|---|
| `nocturnum-core` | shared types: point ids, vectors, payloads, filters, errors |
| `nocturnum-storage` | WAL, segments, generation manifest, record storage |
| `nocturnum-index` | flat and HNSW vector indexes, quantization, payload indexes |
| `nocturnum-query` | filter planning, query execution, scoring and fusion |
| `nocturnum-api` | REST and gRPC adapters over one operation layer |
| `nocturnum-server` | the `nocturnum` binary, with the console embedded |

## Running

```bash
cargo run --release -p nocturnum-server -- --data ./data
```

```bash
curl -s -X PUT localhost:6333/v1/collections/notes \
  -H 'Content-Type: application/json' \
  -d '{"vectors": {"dim": 3, "distance": "cosine"}}'
curl -s -X PUT localhost:6333/v1/collections/notes/points \
  -H 'Content-Type: application/json' \
  -d '{"points": [{"id": 1, "vector": [0.1, 0.9, 0.2], "payload": {"tags": ["red"]}}]}'
curl -s -X POST localhost:6333/v1/collections/notes/points/search \
  -H 'Content-Type: application/json' \
  -d '{"vector": [0.1, 0.8, 0.3], "limit": 5}'
```

`cargo test --workspace` runs every test, including the crash tests and the HTTP tests.

## Scope

v1 runs on a single node with its own REST API (gRPC follows), listening on loopback port 6333 by default, and a write is acknowledged once it is on disk unless the client asks for less.

The full set of v1 decisions is in [`docs/decisions/0001-v1-scope.md`](docs/decisions/0001-v1-scope.md).

## How Qdrant works

The study behind this project is in [`docs/how-qdrant-works.md`](docs/how-qdrant-works.md), with the five detailed reports it draws on in [`docs/research/`](docs/research/).

## Licence

Apache-2.0 is the working choice, matching Qdrant and most of the Rust ecosystem, and is confirmed before the first public release.
