//! Distance kernels against plain scalar loops, and a full flat scan.
//!
//! Run with `cargo bench -p nocturnum-index`. A scalar float sum cannot be vectorised, because
//! reordering it changes the result, so the kernels only beat the scalar loops if their
//! separate accumulators were turned into SIMD.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use nocturnum_core::Distance;
use nocturnum_core::vector::{dot, manhattan, squared_euclid};
use nocturnum_index::flat::search_rows;

fn data(n: usize, seed: u32) -> Vec<f32> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x % 2000) as f32 / 1000.0 - 1.0
        })
        .collect()
}

fn scalar_dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn scalar_l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn scalar_l1(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum()
}

type Kernel = fn(&[f32], &[f32]) -> f32;

fn kernels(c: &mut Criterion) {
    let pairs: [(&str, Kernel, Kernel); 3] = [
        ("dot", dot, scalar_dot),
        ("l2", squared_euclid, scalar_l2),
        ("l1", manhattan, scalar_l1),
    ];
    for (name, kernel, scalar) in pairs {
        let mut g = c.benchmark_group(format!("kernel/{name}"));
        for dim in [128usize, 768, 1536] {
            let (a, b) = (data(dim, 1), data(dim, 2));
            g.throughput(Throughput::Elements(dim as u64));
            g.bench_with_input(BenchmarkId::new("nocturnum", dim), &dim, |bench, _| {
                bench.iter(|| kernel(black_box(&a), black_box(&b)))
            });
            g.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bench, _| {
                bench.iter(|| scalar(black_box(&a), black_box(&b)))
            });
        }
        g.finish();
    }
}

fn scan(c: &mut Criterion) {
    let (n, dim) = (10_000usize, 768usize);
    let vectors = data(n * dim, 3);
    let query = data(dim, 4);
    let mut g = c.benchmark_group("flat_scan_10k_768");
    g.throughput(Throughput::Elements(n as u64));
    for distance in [Distance::Dot, Distance::Euclid] {
        g.bench_function(format!("{distance:?}_top10"), |bench| {
            bench.iter(|| {
                search_rows(
                    distance,
                    black_box(&vectors),
                    dim,
                    black_box(&query),
                    10,
                    0..n as u32,
                )
            })
        });
    }
    g.finish();
}

criterion_group!(benches, kernels, scan);
criterion_main!(benches);
