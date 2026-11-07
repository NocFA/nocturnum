# 0001: v1 scope

Decided by the owner on 2026-10-10, after reading `docs/how-qdrant-works.md` and the five research reports.

## API: our own

nocturnum v1 has its own API rather than a Qdrant wire-compatible subset, so existing `qdrant-client` libraries and integrations such as LangChain do not work against it unchanged.

The concepts stay familiar (collections, points with ids, vectors and JSON payloads, filters, similarity queries), which keeps migrating from Qdrant a matter of changing client calls rather than rethinking the data, and the API is free to drop Qdrant's sharp edges:

- a field condition with several predicates means all of them (AND), where Qdrant ORs them,
- an empty `should` has one meaning everywhere,
- the acknowledgement level of every write is explicit (see below),
- unsupported options fail with a clear error instead of being ignored.

REST with JSON is the first surface, and gRPC exposes the same operations through the same operation layer in `nocturnum-api`, after REST.

The console talks to the same public API as every other client, with no private endpoints.

A Qdrant compatibility shim is not ruled out later, as a separate adapter over the operation layer, but nothing in v1 is shaped around it.

## Clustering: single node

v1 runs on one node.

Shard ids exist in the on-disk format from the start, with every v1 collection using shard 0, so clustering can be added later without migrating data.

## Write acknowledgement: durable by default

Every write request carries an acknowledgement level, and the default is `durable`.

| Level | The response means | Lost on |
|---|---|---|
| `accepted` | validated and appended to the WAL buffer | process crash or power loss |
| `applied` | appended to the WAL and applied, so search sees it | power loss before the next background fsync |
| `durable` (default) | the WAL record is fsynced to disk and the write is applied | nothing, as long as the disk honours flushes |

`durable` uses group commit, so concurrent writers share one fsync rather than paying for one each.

On macOS a plain `fsync` does not reach the platter or flash, so `durable` uses `F_FULLFSYNC` there, and `fdatasync` on Linux.

One request is one WAL record, so a batch inside a single request is all or nothing after a crash, while separate requests are independent.

Every row of that table is a tested claim: crash and fault-injection tests prove each level before the docs or the API promise it.

## Ports: 6333 and 6334

REST and the console listen on 6333, and gRPC takes 6334 once it lands, the same numbers Qdrant uses.

Both bind to loopback by default and need an explicit setting to listen elsewhere, so the console is reached at a tailnet name such as `http://my-server:6333/` once the bind is opened to the tailnet.

A Qdrant client pointed at nocturnum gets clear errors rather than silent misbehaviour, because the API differs, and running nocturnum beside a Qdrant on the same box means changing one of them.

## Licence: Apache-2.0

Apache-2.0, as set in the workspace `Cargo.toml`, was confirmed on 2026-11-07 ahead of the first public release, with the full text in `LICENSE`.
