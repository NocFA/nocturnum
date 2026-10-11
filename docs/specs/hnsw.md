# HNSW graph file, version 1

A segment's graph lives in `graph.bin` inside the segment directory, written once with the rest of the segment and never changed.
The code is `crates/nocturnum-index/src/hnsw/file.rs`, and how the graph is built and searched is in `crates/nocturnum-index/src/hnsw.rs`.

All integers are little-endian.

## Layout

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic, `nochnsw1` |
| 8 | 4 | format version, 1 |
| 12 | 1 | distance: 0 cosine, 1 dot, 2 euclid, 3 manhattan |
| 13 | 3 | zero |
| 16 | 4 | vector dimension |
| 20 | 4 | `m`, links per node on upper layers, layer 0 allows `2m` |
| 24 | 4 | `ef_construct`, the beam width the graph was built with |
| 28 | 8 | the seed node levels were drawn from |
| 36 | 4 | `n`, the number of nodes, one per segment row |
| 40 | 4 | the entry node, `0xFFFFFFFF` when `n` is 0 |
| 44 | 4 | the number of upper layers, equal to the highest node level |

Then, in order:

1. `n` bytes, each node's level.
2. `n + 1` `u32` offsets into the layer 0 links, starting at 0 and never decreasing.
3. The layer 0 links, `u32` node numbers, node `i` owning `links[offset[i]..offset[i + 1]]`.
4. For each upper layer from 1 up: a `u32` count of nodes on it, then for each node in increasing order its number, a `u32` link count and that many `u32` links.
5. A `u32` CRC32C of every byte before it.

## What reading checks

A file is refused unless all of these hold, so a search never reads out of bounds or loops on a malformed graph:

- the checksum, magic and version match,
- the distance, dimension and node count match the segment it belongs to,
- `m` is between 2 and 128 and no node level exceeds 16, the highest level equalling the number of upper layers,
- the entry is a node on the top layer, or absent exactly when there are no nodes,
- layer 0 offsets start at 0, never decrease and end at the number of links, which is at most `n * (2m + 1)`,
- every link names another node, never the node itself and never the same node twice in one list, and upper-layer links name nodes on that layer,
- each upper layer lists exactly the nodes whose level reaches it, in increasing order, each with at most `m` links, and its count is checked against the bytes left before anything is reserved for it,
- no bytes follow the last layer.

Before the file is read at all, its size is checked against a bound worked out from the header's `m` and the segment's row count, about `rows * (72m + 132)` bytes, so a damaged file cannot make opening a segment read or allocate gigabytes.

Layer 0 lists may hold more than `2m` links, because the build links every node a search from the entry could not otherwise reach from its nearest reachable node, which adds at most one link per node.
Those links run one way, so a search seeds its layer 0 beam with the entry as well as the node its descent reached.

## Levels

Node `i`'s level is `floor(-ln(U) / ln(m))`, capped at 16, where `U = ((h >> 11) + 1) / 2^53` and `h` is splitmix64's finaliser applied to `seed XOR i`.
The levels are stored rather than recomputed, so a reader never depends on that formula.
