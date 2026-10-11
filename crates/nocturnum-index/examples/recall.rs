//! Recall harness for the HNSW graph on a public dataset in the texmex `.fvecs` format, such
//! as SIFT or GloVe converted to it.
//!
//! ```text
//! cargo run --release -p nocturnum-index --example recall -- <base.fvecs> <query.fvecs> [euclid|cosine|dot] [m] [ef_construct]
//! ```
//!
//! It builds a graph over the base vectors, works out the true top 10 of every query with
//! exact search, and prints recall@10 and queries per second at a range of `ef` values.

use std::collections::HashSet;
use std::time::Instant;

use nocturnum_core::Distance;
use nocturnum_index::flat::search_rows;
use nocturnum_index::{Hnsw, HnswConfig};
use rayon::prelude::*;

/// Reads an `.fvecs` file: each vector is a little-endian `i32` dimension followed by that many
/// `f32` values.
fn read_fvecs(path: &str) -> (Vec<f32>, usize) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    let dim = i32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")) as usize;
    let stride = 4 + dim * 4;
    assert_eq!(
        bytes.len() % stride,
        0,
        "{path} is not whole vectors of {dim} dimensions"
    );
    let mut out = Vec::with_capacity(bytes.len() / stride * dim);
    for row in bytes.chunks_exact(stride) {
        assert_eq!(
            i32::from_le_bytes(row[..4].try_into().expect("4")) as usize,
            dim
        );
        out.extend(
            row[4..]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c)),
        );
    }
    (out, dim)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, base, queries, rest @ ..] = &args[..] else {
        eprintln!(
            "usage: recall <base.fvecs> <query.fvecs> [euclid|cosine|dot] [m] [ef_construct]"
        );
        std::process::exit(2);
    };
    let distance = match rest.first().map(String::as_str) {
        None | Some("euclid") => Distance::Euclid,
        Some("cosine") => Distance::Cosine,
        Some("dot") => Distance::Dot,
        Some(other) => panic!("unknown distance {other}"),
    };
    let config = HnswConfig {
        m: rest
            .get(1)
            .map_or(16, |m| m.parse().expect("m is a number")),
        ef_construct: rest
            .get(2)
            .map_or(100, |e| e.parse().expect("ef_construct is a number")),
        ..HnswConfig::default()
    };
    let prepare = |(raw, dim): (Vec<f32>, usize)| -> (Vec<f32>, usize) {
        let v = raw
            .chunks(dim)
            .flat_map(|v| {
                distance
                    .prepare(dim, v.to_vec())
                    .expect("finite, non-zero vectors")
            })
            .collect();
        (v, dim)
    };
    let (base, dim) = prepare(read_fvecs(base));
    let (queries, qdim) = prepare(read_fvecs(queries));
    assert_eq!(dim, qdim, "base and queries differ in dimension");
    let n = base.len() / dim;
    println!(
        "{n} base vectors, {} queries, {dim} dimensions, {distance:?}, {config:?}",
        queries.len() / dim
    );

    let started = Instant::now();
    let graph = Hnsw::build(distance, &base, dim, config);
    println!("built in {:.1} s", started.elapsed().as_secs_f64());

    let k = 10;
    let truth: Vec<HashSet<u32>> = queries
        .par_chunks(dim)
        .map(|q| {
            search_rows(distance, &base, dim, q, k, 0..n as u32)
                .iter()
                .map(|s| s.key)
                .collect()
        })
        .collect();
    println!("{:>6} {:>10} {:>12}", "ef", "recall@10", "queries/s");
    for ef in [16, 32, 64, 128, 256, 512] {
        let started = Instant::now();
        let mut hits = 0;
        for (q, want) in queries.chunks(dim).zip(&truth) {
            let got = graph
                .search(&base, q, k, ef, |_| Ok::<_, ()>(true))
                .expect("no filter");
            hits += got.iter().filter(|s| want.contains(&s.key)).count();
        }
        let secs = started.elapsed().as_secs_f64();
        let count = truth.len();
        println!(
            "{ef:>6} {:>10.4} {:>12.0}",
            hits as f64 / (count * k) as f64,
            count as f64 / secs
        );
    }
}
