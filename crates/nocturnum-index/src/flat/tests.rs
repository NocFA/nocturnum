use super::*;
use proptest::prelude::*;

const METRICS: [Distance; 4] = [
    Distance::Cosine,
    Distance::Dot,
    Distance::Euclid,
    Distance::Manhattan,
];

/// The plainest possible top-k: score everything, sort, cut.
fn naive<K: Ord + Copy>(scored: &[(K, f32)], k: usize) -> Vec<Scored<K>> {
    let mut all: Vec<_> = scored
        .iter()
        .map(|&(key, s)| Scored {
            key,
            score: if s.is_nan() { f32::NEG_INFINITY } else { s },
        })
        .collect();
    all.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap()
            .then(a.key.cmp(&b.key))
    });
    all.truncate(k);
    all
}

/// The metric computed in f64 straight from its definition, higher is better.
fn exact(distance: Distance, a: &[f32], b: &[f32]) -> f64 {
    let pairs = a.iter().zip(b).map(|(x, y)| (*x as f64, *y as f64));
    match distance {
        Distance::Cosine | Distance::Dot => pairs.map(|(x, y)| x * y).sum(),
        Distance::Euclid => -pairs.map(|(x, y)| (x - y) * (x - y)).sum::<f64>(),
        Distance::Manhattan => -pairs.map(|(x, y)| (x - y).abs()).sum::<f64>(),
    }
}

fn prepared(distance: Distance, dim: usize, raw: &[f32]) -> Vec<f32> {
    raw.chunks(dim)
        .flat_map(|v| distance.prepare(dim, v.to_vec()).unwrap())
        .collect()
}

#[derive(Debug)]
struct Case {
    distance: Distance,
    dim: usize,
    vectors: Vec<f32>,
    query: Vec<f32>,
    k: usize,
    allow: Vec<u32>,
}

fn case() -> impl Strategy<Value = Case> {
    // `distinct` below the row count makes rows repeat, so exact ties between keys are common.
    let shape = (
        0..4usize,
        1..40usize,
        0..120usize,
        0..30usize,
        1..6usize,
        any::<bool>(),
    );
    shape.prop_flat_map(|(m, dim, n, k, distinct, repeat)| {
        let distance = METRICS[m];
        let distinct = if repeat {
            distinct.min(n.max(1))
        } else {
            n.max(1)
        };
        // No value is zero, so no vector is the zero vector cosine rejects, and shrinking
        // stops at 0.01 instead of reaching it.
        let value = prop_oneof![
            0.01f32..4.0,
            -4.0f32..-0.01,
            (-3i8..3).prop_map(|x| x as f32 + 0.5),
        ];
        (
            proptest::collection::vec(value.clone(), distinct * dim),
            proptest::collection::vec(value, dim),
            proptest::collection::vec(any::<bool>(), n),
        )
            .prop_map(move |(pool, q, mask)| {
                let raw: Vec<f32> = (0..n)
                    .flat_map(|r| pool[(r % distinct) * dim..][..dim].iter().copied())
                    .collect();
                Case {
                    distance,
                    dim,
                    vectors: prepared(distance, dim, &raw),
                    query: distance.prepare(dim, q).unwrap(),
                    k,
                    allow: (0..n as u32).filter(|&r| mask[r as usize]).collect(),
                }
            })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// The heap returns exactly what sorting every score returns, with or without an
    /// allow-list and in any row order, and the iterator form agrees with the slice form.
    #[test]
    fn matches_sorting_every_score(c in case()) {
        let n = c.vectors.len() / c.dim;
        // Row order must not matter, and reversed rows reach a tie with the larger key first.
        let all: Vec<u32> = (0..n as u32).collect();
        let reversed: Vec<u32> = all.iter().rev().copied().collect();
        for rows in [all, reversed, c.allow.clone()] {
            let scored: Vec<_> = rows
                .iter()
                .map(|&r| {
                    let v = &c.vectors[r as usize * c.dim..][..c.dim];
                    (r, c.distance.score(&c.query, v))
                })
                .collect();
            let want = naive(&scored, c.k);
            let got = search_rows(c.distance, &c.vectors, c.dim, &c.query, c.k, rows.iter().copied());
            prop_assert_eq!(&got, &want);
            let via_iter = search(
                c.distance,
                &c.query,
                c.k,
                rows.iter().map(|&r| (r, &c.vectors[r as usize * c.dim..][..c.dim])),
            );
            prop_assert_eq!(&via_iter, &want);
        }
    }

    /// Against the metric worked out in f64, nothing left out scores clearly better than
    /// anything kept, so float rounding in the kernels can only swap near ties.
    #[test]
    fn nothing_better_is_left_out(c in case()) {
        let n = c.vectors.len() / c.dim;
        let got = search_rows(c.distance, &c.vectors, c.dim, &c.query, c.k, 0..n as u32);
        prop_assert_eq!(got.len(), c.k.min(n));
        let truth = |r: u32| exact(c.distance, &c.query, &c.vectors[r as usize * c.dim..][..c.dim]);
        let Some(worst_kept) = got.iter().map(|s| truth(s.key)).reduce(f64::min) else {
            return Ok(());
        };
        let kept: std::collections::HashSet<_> = got.iter().map(|s| s.key).collect();
        let tolerance = 1e-4 * (1.0 + worst_kept.abs()) * c.dim as f64;
        for r in (0..n as u32).filter(|r| !kept.contains(r)) {
            prop_assert!(
                truth(r) <= worst_kept + tolerance,
                "row {} scores {} but the worst kept scores {}", r, truth(r), worst_kept
            );
        }
        for s in &got {
            let t = truth(s.key);
            prop_assert!((s.score as f64 - t).abs() <= 1e-4 * (1.0 + t.abs()) * c.dim as f64);
        }
    }
}

#[test]
fn ties_are_broken_by_the_smaller_key() {
    let mut top = TopK::new(2);
    for key in [5u32, 1, 9, 3] {
        top.push(key, 1.0);
    }
    let keys: Vec<_> = top.into_sorted_vec().iter().map(|s| s.key).collect();
    assert_eq!(keys, [1, 3]);
}

#[test]
fn negative_zero_ties_with_zero() {
    let mut top = TopK::new(1);
    top.push(2u32, 0.0);
    top.push(1, -0.0);
    assert_eq!(top.into_sorted_vec()[0].key, 1);
}

#[test]
fn nan_ranks_last() {
    let mut top = TopK::new(2);
    top.push(1u32, f32::NAN);
    top.push(2, -1e30);
    top.push(3, f32::NAN);
    let got = top.into_sorted_vec();
    assert_eq!(
        got[0],
        Scored {
            key: 2,
            score: -1e30
        }
    );
    assert_eq!(
        got[1],
        Scored {
            key: 1,
            score: f32::NEG_INFINITY
        }
    );
}

#[test]
fn k_zero_and_k_past_the_end() {
    let vectors = [1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
    assert!(search_rows(Distance::Dot, &vectors, 2, &[1.0, 0.0], 0, 0..3).is_empty());
    let all = search_rows(Distance::Dot, &vectors, 2, &[1.0, 0.0], 10, 0..3);
    let keys: Vec<_> = all.iter().map(|s| s.key).collect();
    assert_eq!(keys, [0, 2, 1]);
    assert!(search_rows(Distance::Dot, &vectors, 2, &[1.0, 0.0], 3, []).is_empty());
}

#[test]
fn threshold_appears_once_full() {
    let mut top = TopK::new(2);
    top.push(1u32, 3.0);
    assert_eq!(top.threshold(), None);
    top.push(2, 5.0);
    assert_eq!(top.threshold(), Some(3.0));
    top.push(3, 4.0);
    assert_eq!(top.threshold(), Some(4.0));
    assert_eq!(top.len(), 2);
}

#[test]
fn euclid_finds_the_nearest_point() {
    // A small grid where the answer is obvious.
    let vectors: Vec<f32> = (0..10).flat_map(|i| [i as f32, 0.0]).collect();
    let got = search_rows(Distance::Euclid, &vectors, 2, &[6.2, 0.0], 3, 0..10);
    let keys: Vec<_> = got.iter().map(|s| s.key).collect();
    assert_eq!(keys, [6, 7, 5]);
    assert!((Distance::Euclid.to_user(got[0].score) - 0.2).abs() < 1e-5);
}

#[test]
#[should_panic(expected = "wrong dimension")]
fn a_query_of_the_wrong_dimension_panics() {
    search_rows(Distance::Dot, &[1.0, 2.0], 2, &[1.0], 1, 0..1);
}

#[test]
fn an_interrupt_stops_a_long_scan() {
    let dim = 2;
    let rows = 3 * INTERRUPT_EVERY as u32;
    let vectors: Vec<f32> = (0..rows).flat_map(|i| [i as f32, 1.0]).collect();
    let mut all = roaring::RoaringBitmap::new();
    all.insert_range(0..rows);
    let bounds = Bounds {
        sure: all.clone(),
        maybe: all,
    };
    let mut calls = 0;
    let got = search_filtered_until(
        Distance::Dot,
        &vectors,
        dim,
        &[1.0, 0.0],
        3,
        &bounds,
        |_| Ok::<_, &str>(true),
        || {
            calls += 1;
            if calls > 2 { Err("stop") } else { Ok(()) }
        },
    );
    assert_eq!(got, Err("stop"));
    assert_eq!(calls, 3);
}
