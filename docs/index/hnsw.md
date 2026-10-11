# HNSW graph index

Segments with at least 10 MiB of vectors get a hierarchical navigable small world graph, built with the rest of the segment before it is renamed into place and stored as `graph.bin` beside it, see [`docs/specs/hnsw.md`](../specs/hnsw.md) for the file.
The code is `crates/nocturnum-index/src/hnsw.rs`, written from Malkov and Yashunin's paper.

## How it is built

- Every row is a node with a level drawn from a seeded hash, so a node appears on layers `0..=L` and upper layers thin out by a factor of `m`.
- Each insert walks greedily down the upper layers, runs a beam search of width `ef_construct` on each of its own layers, and keeps links chosen by the diversity heuristic, which takes a candidate only when it is closer to the new node than to every link already taken, filling spare slots with the nearest rejected ones.
- Neighbours link back, trimming their lists with the same heuristic when they pass `m` links, or `2m` on layer 0.
- Equal scores break by a hash of the pair while building, because breaking them by row sent every duplicate's links to the same low rows and stranded the rest.
- A last pass links every node a search from the entry cannot reach on layer 0 from its nearest reachable node, since trimming can leave a node with no way in, which dot product does to about 4% of nodes at `m = 6` on random data, because it is not a metric.
- The first 256 rows go in one at a time and the rest in parallel with rayon, each node's links behind its own lock.

Defaults are `m = 16` and `ef_construct = 100`, and a search's beam defaults to 128 and never drops below `offset + limit`.

## Filtered search

The planner (`crates/nocturnum-query/src/plan.rs`) scores the candidate rows directly when few can match and walks the graph when many can.
On the graph, rows that fail the filter are still walked through, so a search can cross them, but never returned, and the filter only reads a payload for rows that score well enough to enter the results.
The layer 0 beam starts from both the node the descent reached and the entry, because the repair pass only promises every row can be reached from the entry, which keeps a lone matching row findable.

## Recall

recall@10 is the share of the true ten nearest points, from exact search, that the graph returns.

**SIFT-1M** (1,000,000 vectors of 128 dimensions, 10,000 queries, euclid), on the M1 Pro with defaults, built in 55 s on 10 cores with a peak of 1 GB of memory:

| `ef` | recall@10 | queries/s on one thread |
|---|---|---|
| 16 | 0.837 | 14,300 |
| 32 | 0.922 | 9,200 |
| 64 | 0.971 | 5,400 |
| 128 | 0.991 | 3,000 |
| 256 | 0.998 | 1,700 |
| 512 | 1.000 | 960 |

**SIFT-small** (10,000 vectors) reaches 0.980 at `ef` 16 and 1.000 from 128, built in 0.3 s.

**Uniform random data** in 128 dimensions is about the hardest case, since it has no structure for a graph to use: 10,000 points reach 0.86 at `ef` 100 and 0.95 at `ef` 200.
hnswlib, the reference implementation, measured 0.82 and 0.95 on the same shape of data with the same `m` and `ef_construct`, so the gap to SIFT is the data rather than the graph.

Run it on any `.fvecs` dataset with:

```bash
cargo run --release -p nocturnum-index --example recall -- base.fvecs query.fvecs euclid
```

The texmex SIFT files are at `ftp://ftp.irisa.fr/local/texmex/corpus/`, and the tests in `crates/nocturnum-index/src/hnsw/tests.rs` check recall on random data on every run, unfiltered and with filters passing 1%, 10% and 50% of rows.
