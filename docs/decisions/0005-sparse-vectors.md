# 0005: sparse vectors ride beside the dense vector, inverted per segment

Decided while building sparse vectors on 2026-10-12, an implementation choice within the v1 scope rather than an owner decision.

## What

- A point can carry one sparse vector beside its dense one, optional and unnamed, since v1 has no named vectors.
- The WAL logs it in a new upsert record, tag 7, written only when a point in the write has one, so a log from a collection that never uses them reads the same as before.
- A segment holding any sparse vector writes it to `sparse.bin`, (dimension, weight) pairs per row, with `sparse_offsets.bin` beside it, and is marked format 2, so a build from before sparse vectors refuses it rather than dropping them, while a segment without any stays format 1.
- Each segment's inverted index, the rows holding each dimension with their weights, is derived when the segment opens, like the payload indexes in decision 0003, and the memtable's sparse vectors are scored directly.
- `idf` is a query option counted over the live points when the search runs, as text search counts BM25's statistics in decision 0004, so no corpus statistics are stored.
- An empty sparse vector is stored as none, so having a sparse vector always means having at least one dimension.

The code is `crates/nocturnum-core/src/sparse.rs` for the type, `crates/nocturnum-storage/src/{op,segment}.rs` for the formats, `crates/nocturnum-index/src/sparse.rs` for the index and `crates/nocturnum-query/src/collection/sparse.rs` for the search.

## Why

Qdrant stores sparse vectors as named vector spaces with mutable and compressed inverted indexes of their own, and gathers IDF statistics per shard.

nocturnum's segments never change once written, so an index derived from the stored pairs stays right for the segment's life and needs no persistence, ordering or repair, and counting IDF when the search runs keeps it exact after every delete.

Marking only segments that hold sparse vectors with the new format keeps downgrades possible for collections that do not use them, while one that does fails loudly on an older build instead of quietly losing data.

## What it costs

- **Startup time and memory:** opening a segment decodes every sparse vector to build its index, which lives in RAM at 8 bytes per stored weight plus, for every distinct dimension, a hash map entry and two growable arrays, so many rarely used dimensions cost far more than their weights.
- **Query time:** a search visits the postings of each query dimension in every segment and the memtable's sparse vectors in full, with no pruning by the largest weights, which is exact but grows with how common the query's dimensions are.
- **One sparse vector per point:** a second model's output, or several, needs named vectors, which are a later task.
