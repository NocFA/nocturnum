use std::collections::HashSet;

use super::*;
use crate::flat::search_rows;

/// Uniform values in [-1, 1) from a fixed seed.
fn data(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut x = mix(seed) | 1;
    (0..n * dim)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        })
        .collect()
}

fn prepared(distance: Distance, dim: usize, raw: Vec<f32>) -> Vec<f32> {
    raw.chunks(dim)
        .flat_map(|v| distance.prepare(dim, v.to_vec()).unwrap())
        .collect()
}

/// Mean recall@k of the graph against exact search over `queries`, with rows passing
/// `allowed` the only ones that count.
fn recall(
    g: &Hnsw,
    vectors: &[f32],
    queries: &[f32],
    k: usize,
    ef: usize,
    allowed: impl Fn(u32) -> bool,
) -> f64 {
    let dim = g.dim;
    let n = vectors.len() / dim;
    let mut total = 0.0;
    let count = queries.len() / dim;
    for q in queries.chunks(dim) {
        let exact: HashSet<u32> = search_rows(
            g.distance,
            vectors,
            dim,
            q,
            k,
            (0..n as u32).filter(|&r| allowed(r)),
        )
        .iter()
        .map(|s| s.key)
        .collect();
        let got = g
            .search(vectors, q, k, ef, |r| Ok::<_, ()>(allowed(r)))
            .unwrap();
        assert!(
            got.iter().all(|s| allowed(s.key)),
            "a filtered-out row came back"
        );
        let hits = got.iter().filter(|s| exact.contains(&s.key)).count();
        total += hits as f64 / exact.len().max(1) as f64;
    }
    total / count as f64
}

/// Uniform random data in 128 dimensions has no structure for a graph to use, which makes
/// it about the hardest case: hnswlib, the reference implementation, measured 0.82 at ef 100
/// and 0.95 at ef 200 on the same shape of data, against 0.86 and 0.95 here, see
/// `docs/index/hnsw.md`.
#[test]
fn recall_at_10_on_random_128_dim_data() {
    let (n, dim) = (10_000, 128);
    let vectors = data(n, dim, 1);
    let queries = data(100, dim, 2);
    let g = Hnsw::build(Distance::Euclid, &vectors, dim, HnswConfig::default());
    let r = recall(&g, &vectors, &queries, 10, 200, |_| true);
    eprintln!("recall@10 at ef 200: {r:.3}");
    assert!(r >= 0.93, "recall@10 was {r:.3}");
}

#[test]
fn filtered_recall_holds_at_1_10_and_50_percent() {
    let (n, dim) = (10_000, 32);
    let vectors = prepared(Distance::Cosine, dim, data(n, dim, 3));
    let queries = prepared(Distance::Cosine, dim, data(50, dim, 4));
    let g = Hnsw::build(Distance::Cosine, &vectors, dim, HnswConfig::default());
    for percent in [1u32, 10, 50] {
        // Spread by a hash so the allowed rows are scattered through the graph.
        let allowed = |r: u32| (mix(u64::from(r)) % 100) < u64::from(percent);
        let r = recall(&g, &vectors, &queries, 10, 100, allowed);
        eprintln!("filtered recall@10 at {percent}%: {r:.3}");
        assert!(r >= 0.95, "recall@10 at {percent}% was {r:.3}");
    }
}

#[test]
fn every_metric_finds_its_neighbours() {
    let (n, dim) = (1_500, 16);
    for distance in [
        Distance::Cosine,
        Distance::Dot,
        Distance::Euclid,
        Distance::Manhattan,
    ] {
        let vectors = prepared(distance, dim, data(n, dim, 5));
        let queries = prepared(distance, dim, data(20, dim, 6));
        let g = Hnsw::build(distance, &vectors, dim, HnswConfig::default());
        let r = recall(&g, &vectors, &queries, 10, 64, |_| true);
        assert!(r >= 0.95, "{distance:?} recall@10 was {r:.3}");
    }
}

#[test]
fn a_counted_build_counts_every_row() {
    let vectors = data(700, 8, 5);
    let inserted = AtomicU64::new(0);
    let g = Hnsw::build_counted(
        Distance::Euclid,
        &vectors,
        8,
        HnswConfig::default(),
        &inserted,
    );
    assert_eq!(inserted.load(AtomicOrdering::Acquire), 700);
    assert_eq!(g.len(), 700);
}

#[test]
fn serial_builds_repeat_exactly() {
    let vectors = data(600, 8, 7);
    let a = Hnsw::build_serial(Distance::Euclid, &vectors, 8, HnswConfig::default());
    let b = Hnsw::build_serial(Distance::Euclid, &vectors, 8, HnswConfig::default());
    assert_eq!(a, b);
}

fn unreachable(g: &Hnsw) -> usize {
    let n = g.len();
    let mut seen = vec![false; n];
    let mut stack = vec![g.entry.unwrap()];
    seen[stack[0] as usize] = true;
    while let Some(node) = stack.pop() {
        g.with_links(node, 0, |links| {
            for &l in links {
                if !std::mem::replace(&mut seen[l as usize], true) {
                    stack.push(l);
                }
            }
        });
    }
    seen.iter().filter(|s| !**s).count()
}

/// Trimming strands some nodes, most of all under dot product, which is not a metric, so
/// rows with small norms are rarely anyone's nearest, and the repair pass links them back.
#[test]
fn every_node_is_reachable_on_layer_0() {
    for (distance, m) in [
        (Distance::Euclid, 16),
        (Distance::Dot, 6),
        (Distance::Dot, 16),
    ] {
        let vectors = data(3_000, 24, 8);
        let config = HnswConfig {
            m,
            ..HnswConfig::default()
        };
        let g = Hnsw::build(distance, &vectors, 24, config);
        assert_eq!(unreachable(&g), 0, "{distance:?} with m {m}");
    }
}

#[test]
fn links_respect_their_limits_and_layers() {
    let vectors = data(2_000, 8, 9);
    let config = HnswConfig {
        m: 6,
        ..HnswConfig::default()
    };
    // A true metric built in order needs no repair links here, so every list stays within
    // its limit, while a parallel build can occasionally need one.
    let g = Hnsw::build_serial(Distance::Euclid, &vectors, 8, config);
    for node in 0..g.len() as u32 {
        let level = g.levels[node as usize];
        for layer in 0..=level {
            g.with_links(node, layer, |links| {
                let limit = if layer == 0 { config.m0() } else { config.m };
                assert!(
                    links.len() <= limit,
                    "node {node} has {} links on {layer}",
                    links.len()
                );
                for &l in links {
                    assert_ne!(l, node, "node {node} links to itself");
                    assert!(g.levels[l as usize] >= layer, "a link leaves layer {layer}");
                }
            });
        }
    }
    let top = g.levels[g.entry.unwrap() as usize];
    assert_eq!(
        top,
        *g.levels.iter().max().unwrap(),
        "the entry is not on the top layer"
    );
}

#[test]
fn levels_thin_out_by_m_per_layer() {
    let config = HnswConfig::default();
    let n = 100_000u32;
    let above: Vec<usize> = (1..4u8)
        .map(|l| (0..n).filter(|&r| config.level(r) >= l).count())
        .collect();
    // About n / 16, n / 256 and n / 4096.
    assert!((5_500..7_000).contains(&above[0]), "{above:?}");
    assert!((300..500).contains(&above[1]), "{above:?}");
    assert!((10..45).contains(&above[2]), "{above:?}");
}

#[test]
fn small_and_degenerate_graphs() {
    let empty = Hnsw::build(Distance::Dot, &[], 3, HnswConfig::default());
    assert!(
        empty
            .search(&[], &[1.0, 0.0, 0.0], 5, 10, |_| Ok::<_, ()>(true))
            .unwrap()
            .is_empty()
    );

    let one = Hnsw::build(Distance::Dot, &[1.0, 2.0], 2, HnswConfig::default());
    let got = one
        .search(&[1.0, 2.0], &[1.0, 0.0], 5, 10, |_| Ok::<_, ()>(true))
        .unwrap();
    assert_eq!(got.iter().map(|s| s.key).collect::<Vec<_>>(), [0]);

    // Identical vectors must not trap the search in one corner of the graph.
    let same = vec![0.5f32; 300 * 4];
    let g = Hnsw::build(Distance::Euclid, &same, 4, HnswConfig::default());
    let got = g
        .search(&same, &[0.5; 4], 50, 64, |_| Ok::<_, ()>(true))
        .unwrap();
    assert_eq!(got.len(), 50);
    // Ties come back by row.
    assert_eq!(
        got.iter().map(|s| s.key).collect::<Vec<_>>(),
        (0..50).collect::<Vec<_>>()
    );
}

#[test]
fn errors_from_the_filter_come_back() {
    let vectors = data(100, 4, 10);
    let g = Hnsw::build(Distance::Dot, &vectors, 4, HnswConfig::default());
    let got = g.search(&vectors, &[1.0; 4], 5, 10, |_| Err("unreadable"));
    assert_eq!(got.unwrap_err(), "unreadable");
}

/// Found by the audit: the descent can end in a part of layer 0 that cannot reach every
/// row, so the search also starts from the entry, from which the repair pass guarantees
/// every row can be reached.
#[test]
fn a_lone_allowed_row_outside_the_descended_part_is_found() {
    let rows = [
        [-6.0, 8.0],
        [-7.0, 5.0],
        [4.0, 5.0],
        [10.0, 2.0],
        [2.0, 3.0],
        [9.0, -10.0],
        [2.0, -4.0],
        [4.0, 5.0],
        [7.0, -3.0],
    ];
    let vectors: Vec<f32> = rows.iter().flatten().copied().collect();
    let config = HnswConfig {
        m: 2,
        ef_construct: 2,
        ..HnswConfig::default()
    };
    let g = Hnsw::build_serial(Distance::Euclid, &vectors, 2, config);
    let q = [4.0, 5.0];
    for target in 0..9u32 {
        let got = g
            .search(&vectors, &q, 1, 9, |r| Ok::<_, ()>(r == target))
            .unwrap();
        assert_eq!(got.iter().map(|s| s.key).collect::<Vec<_>>(), [target]);
    }
    let all = g
        .search(&vectors, &q, 9, 512, |_| Ok::<_, ()>(true))
        .unwrap();
    assert_eq!(all.len(), 9);
}
