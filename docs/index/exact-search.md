# Exact search and distance kernels

Exact search scores every candidate and keeps the best `k`, and it is the yardstick every approximate index gets checked against, so its results have to be right and repeatable before they have to be fast.
The code is `crates/nocturnum-index/src/flat.rs`, with the kernels in `crates/nocturnum-core/src/vector.rs`.

## What it promises

- Results come best first, equal scores ordered by the smaller key, so the same data and query always give the same list whatever order the candidates arrive in.
- A NaN score, which only happens when a dot product overflows to infinities of both signs, ranks as negative infinity.
- Memory stays at `k` results however many candidates are scanned, and a candidate that cannot beat the current worst is dropped before touching the heap.
- A filter is applied by passing only the allowed rows, so the payload index and the planner decide what gets scored.

The tests compare the heap with scoring and sorting everything on random data, with repeated rows for exact ties, allow-lists and reversed row order, and check every kept result against the metric worked out in f64.

## Kernels

Each kernel is a plain loop over 16-float chunks with 16 separate accumulators, so the compiler can use SIMD without reordering a float sum, which it is not allowed to do on its own.

`cargo bench -p nocturnum-index` compares them with scalar loops, and on an M1 Pro (one core, NEON) it measured:

| dimensions | dot, nocturnum | dot, scalar | speedup |
|---|---|---|---|
| 128 | 10 ns | 55 ns | 5.5x |
| 768 | 48 ns | 598 ns | 12.5x |
| 1536 | 93 ns | 1.33 µs | 14x |

Squared euclid and manhattan land within 10% of dot.
Eight lanes were about 40% slower than 16 at 1536 dimensions and 32 lanes were slower again, most likely because 16 gives the M1's float units enough independent sums to stay busy while 32 runs short of registers.

A full scan of 10,000 vectors of 768 dimensions for the top 10 takes about 0.65 ms, or 65 ns a vector against 48 ns for the kernel on data already in cache, so the rest is mostly reading 30 MB of vectors from memory.

Hand-written SIMD per CPU and quantized kernels can come later if a benchmark shows a need, and x86 numbers still have to be measured on an x86 Linux machine.
