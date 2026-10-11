# nocturnum

nocturnum is a vector database written in Rust, for storing embeddings with JSON payloads and searching them by similarity with filters, built to run on one small box at home.

It follows the shape of [Qdrant](https://qdrant.tech/), with collections, points, payload filters, HNSW graphs, a write-ahead log and segments, written from scratch as one binary that holds everything a self-hoster needs, the web console included.

- [What works today](#what-works-today)
- [Quick start](#quick-start)
- [Concepts](#concepts)
- [A tour of the API](#a-tour-of-the-api)
- [Writes and durability](#writes-and-durability)
- [Running it for real](#running-it-for-real)
- [Backups](#backups)
- [How it works inside](#how-it-works-inside)
- [Developing](#developing)
- [Roadmap](#roadmap)
- [Where to read more](#where-to-read-more)

## What works today

The engine and the REST server run end to end, with 217 tests passing on macOS and Linux, crash tests included.

| Area | State |
|---|---|
| Storage | a write-ahead log, an in-memory memtable and immutable segments under a manifest, crash-tested at every step |
| Background work | flushing, merging small segments and vacuuming deleted rows, built aside and swapped in |
| Search | exact search and HNSW graphs, with filters planned against payload indexes |
| Filters | `must`, `should`, `must_not` and `min_should`, with match, any, except, range, null and emptiness checks, value counts, ids and nested arrays |
| Payload indexes | keyword, integer, float and bool |
| REST API | collections, points, search, scroll, count, payload edits, indexes and snapshots, specified in [`docs/api/v1.md`](docs/api/v1.md) |
| Security | admin and read-only keys, loopback by default, protection against DNS rebinding and cross-site requests when no keys are set |
| Operations | `/livez`, `/readyz`, Prometheus `/metrics`, clean shutdown on SIGINT or SIGTERM |
| Backups | snapshots with checksums, download and restore, crash-safe swaps |

Scheduled snapshots, the web console, gRPC and quantization are next, see [Roadmap](#roadmap).

## Quick start

You need Rust, with the toolchain pinned in `rust-toolchain.toml` installed by `rustup` on first build.

```bash
git clone https://git.nocfa.net/NocFA/nocturnum.git
cd nocturnum
cargo run --release -p nocturnum-server -- --data ./data
```

The server listens on `http://127.0.0.1:6333` and keeps everything under `./data`.

```bash
curl -s localhost:6333/readyz; echo
```

```json
{"status":"ready"}
```

The `echo` adds the newline the response leaves out, without it zsh shows a `%` at the end of the line.

## Concepts

A **collection** is a named set of points that all share one vector size and one distance metric, such as 768 dimensions compared by cosine.

A **point** has an id, a vector and a payload:

- the **id** is an unsigned 64-bit integer or a UUID,
- the **vector** is a list of floats of the collection's size,
- the **payload** is any JSON object, used for filtering and returned with results.

**Distance** is one of `cosine`, `dot`, `euclid` or `manhattan`.
For `cosine` and `dot` the score is a similarity and higher is closer, while for `euclid` and `manhattan` it is a distance and lower is closer.

A **filter** narrows a search, scroll, count or bulk change to the points whose payload matches, and works on every field, while a **payload index** on a field makes those filters fast.

A **snapshot** is one tar file holding a whole collection, with its SHA-256 beside it, which can be downloaded or restored.

## A tour of the API

These run against the quick-start server, and the responses shown are real ones.
Request bodies must be sent with `Content-Type: application/json`, so a small shell helper saves typing:

```bash
j() { curl -s -H 'Content-Type: application/json' "$@"; echo; }
```

Create a collection of 3-dimensional vectors compared by cosine:

```bash
j -X PUT localhost:6333/v1/collections/notes \
  -d '{"vectors": {"dim": 3, "distance": "cosine"}}'
```

```json
{"created":true}
```

Add three points, each with a payload:

```bash
j -X PUT localhost:6333/v1/collections/notes/points -d '{"points": [
  {"id": 1, "vector": [0.1, 0.9, 0.2], "payload": {"title": "fox", "tags": ["red"], "price": 12.5}},
  {"id": 2, "vector": [0.8, 0.1, 0.1], "payload": {"title": "owl", "tags": ["brown"], "price": 30}},
  {"id": 3, "vector": [0.2, 0.8, 0.3], "payload": {"title": "cardinal", "tags": ["red"], "price": 8}}
]}'
```

```json
{"seq":1,"ack":"durable"}
```

`seq` is the write's place in the log, and `durable` means it was on disk before the server answered.

Find the two closest points:

```bash
j -X POST localhost:6333/v1/collections/notes/points/search \
  -d '{"vector": [0.1, 0.8, 0.3], "limit": 2}'
```

```json
{"points":[{"id":3,"score":0.99357355,"payload":{"price":8,"tags":["red"],"title":"cardinal"}},{"id":1,"score":0.9902891,"payload":{"price":12.5,"tags":["red"],"title":"fox"}}]}
```

Search only red things under 10, returning just the title:

```bash
j -X POST localhost:6333/v1/collections/notes/points/search -d '{
  "vector": [0.1, 0.8, 0.3],
  "filter": {"must": [{"key": "tags", "match": "red"}, {"key": "price", "range": {"lt": 10}}]},
  "with_payload": {"include": ["title"]}
}'
```

```json
{"points":[{"id":3,"score":0.99357355,"payload":{"title":"cardinal"}}]}
```

Count, page through and fetch points:

```bash
j -X POST localhost:6333/v1/collections/notes/points/count \
  -d '{"filter": {"must": [{"key": "tags", "match": "red"}]}}'
j -X POST localhost:6333/v1/collections/notes/points/scroll -d '{"limit": 2}'
j -X POST localhost:6333/v1/collections/notes/points/get -d '{"ids": [2]}'
```

```json
{"count":2}
{"points":[{"id":1,"payload":{"price":12.5,"tags":["red"],"title":"fox"}},{"id":2,"payload":{"price":30,"tags":["brown"],"title":"owl"}}],"next":2}
{"points":[{"id":2,"payload":{"price":30,"tags":["brown"],"title":"owl"}}]}
```

Scroll's `next` goes into `"after"` on the following request, and is `null` on the last page.

Index a field you filter on often, then look at the collection:

```bash
j -X PUT localhost:6333/v1/collections/notes/indexes -d '{"path": "tags", "kind": "keyword"}'
j localhost:6333/v1/collections/notes
```

```json
{"created":true}
{"name":"notes","vectors":{"dim":3,"distance":"cosine"},"points":3,"segments":0,"indexes":[{"path":"tags","kind":"keyword"}]}
```

`segments` is 0 because these points still sit in the memtable, which is flushed to a segment once it holds 32 MiB.

Change payloads, delete points and take a snapshot:

```bash
j -X POST localhost:6333/v1/collections/notes/points/payload/set -d '{"ids": [2], "payload": {"price": 25}}'
j -X POST localhost:6333/v1/collections/notes/points/delete -d '{"filter": {"must": [{"key": "tags", "match": "brown"}]}}'
j -X POST localhost:6333/v1/collections/notes/snapshots
```

Mistakes come back as a status code with an error naming the problem and, where it can, the field:

```json
{"error":{"at":"vector","code":"invalid_vector","message":"vector has 2 dimensions, the collection expects 3"}}
```

Every endpoint, field, limit and error code is in [`docs/api/v1.md`](docs/api/v1.md).

## Writes and durability

Every write takes `?ack=` to say how sure the answer has to be, and defaults to the safest.

| `ack` | The answer means | Lost on |
|---|---|---|
| `accepted` | checked and appended to the log buffer | a process crash or power loss |
| `applied` | appended to the log and applied, so reads see it | power loss in the second before the next background sync |
| `durable` (default) | the log record is on disk and the write is applied | nothing, as long as the disk honours flushes |

Concurrent `durable` writers share one fsync, and macOS gets `F_FULLFSYNC`, since a plain `fsync` there stops short of the drive.

One request is one log record, so after a crash a request is either all there or not there at all.
Each of these promises has a crash test behind it, where a child process is killed at random or at named points mid-write and the result checked against what it acknowledged, all listed in [`docs/storage/durability.md`](docs/storage/durability.md).

When a disk fails an fsync, the collection stops taking writes with `503` until a restart, since nothing can tell what reached the disk, and `/readyz` reports it.

## Running it for real

### Configuration

Settings come from built-in defaults, then a TOML file, then environment variables, then flags, each overriding the one before.

```toml
[server]
host = "127.0.0.1"
port = 6333

[storage]
path = "/var/lib/nocturnum"

[auth]
admin_key = "a long random string"
read_only_key = "another long random string"

[log]
level = "info"
```

```bash
nocturnum --config /etc/nocturnum.toml
nocturnum --print-config     # shows the effective config, keys hidden
```

Keys can only come from the file or the environment, `NOCTURNUM_ADMIN_KEY` and `NOCTURNUM_READ_ONLY_KEY`, since flags show up in the process list.
Every setting, its variable and its flag are in [`docs/server/config.md`](docs/server/config.md).

### Keys and listening

Clients send `Authorization: Bearer <key>`.
The admin key can do everything, and the read-only key can read, search, scroll, count and download snapshots.

With no keys set the server only listens on loopback, and refuses to start on any other address unless `auth.allow_anonymous = true` says that is intended.
Without keys it also turns away requests whose `Host` is an unknown name and writes that come from another website, so a web page open in your browser cannot reach it.

To reach it from other machines on a tailnet, set an admin key and listen on the tailnet address, or list the names it is reached by in `server.allowed_hosts`, as [`docs/server/config.md`](docs/server/config.md) shows.

### Health and metrics

| Endpoint | Answers |
|---|---|
| `GET /livez` | 200 while the process serves requests |
| `GET /readyz` | 200 once every collection is loaded and healthy, 503 with the reason otherwise |
| `GET /metrics` | Prometheus text, needing a key when keys are set |

`/metrics` has request counts and latencies by route, and per collection the points, segments, a failed flag, WAL size, last sequence number and fsync count and time.

The server binds before it loads collections, so `/livez` answers during a long recovery while `/readyz` and the API answer 503 until loading is done.
On SIGINT or SIGTERM it stops taking connections, lets requests in flight finish and closes every collection before exiting.

### The data directory

```text
data/
  LOCK                  held while a server runs, so a second one cannot share it
  collections/<name>/   one collection: its log, segments and manifest
  snapshots/<name>/     <name>-<milliseconds>.snapshot files with a .sha256 beside each
```

Creating, deleting and restoring a collection each work in a hidden directory beside it and rename it into place, so a crash never leaves half a collection, and startup finishes or rolls back whatever a crash interrupted.

## Backups

```bash
j -X POST localhost:6333/v1/collections/notes/snapshots                  # take one
j localhost:6333/v1/collections/notes/snapshots                          # list them
curl -o notes.snapshot localhost:6333/v1/collections/notes/snapshots/<name>   # download
j -X POST localhost:6333/v1/collections/notes/restore -d '{"snapshot": "<name>"}'
```

A snapshot is taken while writes carry on and holds every write acknowledged before it started.
Restoring checks the file against its checksum and unpacks it beside the live collection first, so a damaged snapshot is refused with `400 invalid_snapshot` and changes nothing, then swaps the copies.

Deleting a collection keeps its snapshots, so restoring one brings it back.

## How it works inside

```text
write --> write-ahead log --> memtable in RAM --flush at 32 MiB--> immutable segment
                                                                   (vectors, payloads, payload indexes,
                                                                    an HNSW graph from 10 MiB of vectors)

read  --> the memtable first, then segments from newest to oldest, the newest copy of a point wins

in the background --> merge small segments and vacuum deleted rows, each published by swapping the manifest
```

- **The write-ahead log** takes every write first, in records with checksums, and recovery cuts off a torn record at its end.
- **The memtable** holds recent writes in RAM, and its only durable form is the log, which keeps the on-disk side simple: every file is written once and never changed.
- **A flush** writes the memtable out as a new immutable segment, built in a temporary directory and renamed into place, then publishes a new **manifest** listing it, and only then trims the log.
- **The optimizer** merges small segments once there are more than 8, and rewrites any segment with more than 20% deleted rows, building aside and swapping the manifest the same way.
- **Search** asks each segment for its best matches and merges them, using exact scans for small or tightly filtered sets and the HNSW graph for large ones, and checks HNSW results against exact search in the tests.

The code is split into crates:

| Crate | What it does |
|---|---|
| `nocturnum-core` | shared types: point ids, vectors and distances, payloads, filters, errors |
| `nocturnum-storage` | the log, memtable, segments, manifest, optimizer and snapshots |
| `nocturnum-index` | exact and HNSW vector indexes, payload indexes |
| `nocturnum-query` | collections: filter planning, search, scroll and count across segments |
| `nocturnum-api` | the REST server, auth, metrics and the collection registry |
| `nocturnum-server` | the `nocturnum` binary: config, startup and shutdown |

## Developing

```bash
cargo test --workspace                    # every test, crash tests included
cargo clippy --workspace --all-targets
cargo run -p nocturnum-server             # a debug server on 6333
```

The crash tests run the test binary again as a child process and kill it, and `NOCTURNUM_CRASH_SEED=<seed>` replays a run they print.

Qdrant's Apache-2.0 source was read to understand how it works, and nocturnum's code is written from scratch, never copied or translated from it.

## Roadmap

1. Scheduled snapshots: a cron schedule and keep-last-N per collection.
2. The web console, built into the binary and served on the same port.
3. gRPC on port 6334, over the same operations as REST.
4. Scalar quantization, sparse vectors with BM25, optional TLS and OpenAPI docs.
5. Benchmarks and power-loss tests on real hardware.

Clustering is out of v1, though shard ids are in the on-disk format already so it can come later without a migration.

## Where to read more

| Document | What it covers |
|---|---|
| [`docs/api/v1.md`](docs/api/v1.md) | the REST API in full |
| [`docs/server/config.md`](docs/server/config.md) | running and configuring the server |
| [`docs/storage/durability.md`](docs/storage/durability.md) | what each write promises and the tests behind it |
| [`docs/decisions/`](docs/decisions/) | the decisions behind v1's scope, storage layout and payload indexes |
| [`docs/index/hnsw.md`](docs/index/hnsw.md) | how the HNSW graphs are built and searched |
| [`docs/how-qdrant-works.md`](docs/how-qdrant-works.md) | the study of Qdrant this project started from, with the detailed reports in [`docs/research/`](docs/research/) |

## Licence

Apache-2.0, the same as Qdrant and most of the Rust ecosystem, with the full text in [`LICENSE`](LICENSE) and the project's notice in [`NOTICE`](NOTICE).
