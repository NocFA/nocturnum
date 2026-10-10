//! Dense vectors and the distance metrics that compare them.
//!
//! Inside the engine every comparison produces a **score where higher is better**, whatever the
//! metric, so ranking, heaps and merging never need to know which metric is in use.
//! [`Distance::to_user`] turns that score back into the metric's natural value for the API:
//! a similarity for cosine and dot, and a distance for euclid and manhattan.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A dense vector of 32-bit floats.
pub type DenseVector = Vec<f32>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Distance {
    /// Cosine similarity, stored vectors and queries are normalised to unit length first, so
    /// scoring is a dot product.
    Cosine,
    /// Raw dot product, higher means more similar.
    Dot,
    /// Euclidean (L2) distance, scored internally as the negated squared distance.
    Euclid,
    /// Manhattan (L1) distance, scored internally as the negated distance.
    Manhattan,
}

impl Distance {
    /// Checks a vector for this metric and returns the form the engine stores and scores,
    /// which for cosine is the unit-length copy.
    pub fn prepare(self, dim: usize, mut vector: DenseVector) -> Result<DenseVector> {
        if vector.len() != dim {
            return Err(Error::DimensionMismatch {
                expected: dim,
                got: vector.len(),
            });
        }
        if let Some(index) = vector.iter().position(|x| !x.is_finite()) {
            return Err(Error::NonFiniteVector { index });
        }
        if self == Distance::Cosine {
            let norm = dot(&vector, &vector).sqrt();
            if norm == 0.0 || !norm.is_finite() {
                return Err(Error::ZeroVector);
            }
            vector.iter_mut().for_each(|x| *x /= norm);
        }
        Ok(vector)
    }

    /// Scores two prepared vectors of equal length, higher is better.
    #[inline]
    pub fn score(self, a: &[f32], b: &[f32]) -> f32 {
        debug_assert_eq!(a.len(), b.len());
        match self {
            Distance::Cosine | Distance::Dot => dot(a, b),
            Distance::Euclid => -squared_euclid(a, b),
            Distance::Manhattan => -manhattan(a, b),
        }
    }

    /// Converts an internal score into the value the API reports for this metric.
    pub fn to_user(self, score: f32) -> f32 {
        match self {
            Distance::Cosine | Distance::Dot => score,
            Distance::Euclid => (-score).max(0.0).sqrt(),
            Distance::Manhattan => -score,
        }
    }

    /// Whether the value from [`Distance::to_user`] grows as points get more similar.
    pub fn user_higher_is_better(self) -> bool {
        matches!(self, Distance::Cosine | Distance::Dot)
    }
}

// The kernels below are plain loops over exact-size chunks with separate accumulators, so the
// compiler turns them into SIMD without being allowed to reorder a float sum.
// `cargo bench -p nocturnum-index` compares them with scalar loops: on an M1 Pro 16 lanes ran
// 5 to 14 times faster than scalar, 8 lanes were about 40% slower than 16 at 1536 dimensions
// and 32 lanes slower again.

const LANES: usize = 16;

/// Runs `lane` over the vectors `LANES` at a time with that many accumulators, then `lane` again
/// over the remainder, and sums everything.
#[inline(always)]
fn fold_lanes(a: &[f32], b: &[f32], lane: impl Fn(f32, f32) -> f32) -> f32 {
    let (ca, ra) = a.as_chunks::<LANES>();
    let (cb, rb) = b.as_chunks::<LANES>();
    let mut acc = [0.0f32; LANES];
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..LANES {
            acc[i] += lane(x[i], y[i]);
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| lane(*x, *y)).sum();
    acc.iter().sum::<f32>() + tail
}

#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    fold_lanes(a, b, |x, y| x * y)
}

#[inline]
pub fn squared_euclid(a: &[f32], b: &[f32]) -> f32 {
    fold_lanes(a, b, |x, y| (x - y) * (x - y))
}

#[inline]
pub fn manhattan(a: &[f32], b: &[f32]) -> f32 {
    fold_lanes(a, b, |x, y| (x - y).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1e-4 * (1.0 + a.abs().max(b.abs()))
    }

    // Lengths that exercise both the chunked body and the remainder.
    fn pairs() -> Vec<(Vec<f32>, Vec<f32>)> {
        [1usize, 7, 8, 9, 15, 16, 17, 31, 33, 128]
            .iter()
            .map(|&n| {
                let a = (0..n).map(|i| (i as f32 * 0.37).sin()).collect();
                let b = (0..n).map(|i| (i as f32 * 0.11).cos()).collect();
                (a, b)
            })
            .collect()
    }

    #[test]
    fn kernels_match_naive_maths() {
        for (a, b) in pairs() {
            let naive_dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
            let naive_l2: f32 = a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum();
            let naive_l1: f32 = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum();
            assert!(close(dot(&a, &b), naive_dot));
            assert!(close(squared_euclid(&a, &b), naive_l2));
            assert!(close(manhattan(&a, &b), naive_l1));
        }
    }

    #[test]
    fn cosine_normalises_and_rejects_zero() {
        let v = Distance::Cosine.prepare(2, vec![3.0, 4.0]).unwrap();
        assert!(close(v[0], 0.6) && close(v[1], 0.8));
        assert_eq!(
            Distance::Cosine.prepare(2, vec![0.0, 0.0]),
            Err(Error::ZeroVector)
        );
        // Dot keeps zero vectors, they are valid there.
        assert!(Distance::Dot.prepare(2, vec![0.0, 0.0]).is_ok());
    }

    #[test]
    fn prepare_checks_dimension_and_finiteness() {
        assert_eq!(
            Distance::Dot.prepare(3, vec![1.0]),
            Err(Error::DimensionMismatch {
                expected: 3,
                got: 1
            })
        );
        assert_eq!(
            Distance::Dot.prepare(2, vec![1.0, f32::NAN]),
            Err(Error::NonFiniteVector { index: 1 })
        );
        assert_eq!(
            Distance::Euclid.prepare(1, vec![f32::INFINITY]),
            Err(Error::NonFiniteVector { index: 0 })
        );
    }

    #[test]
    fn higher_score_always_means_closer() {
        let q = [0.0, 0.0];
        let near = [1.0, 0.0];
        let far = [3.0, 4.0];
        for d in [Distance::Euclid, Distance::Manhattan] {
            assert!(d.score(&q, &near) > d.score(&q, &far));
        }
        assert!(close(
            Distance::Euclid.to_user(Distance::Euclid.score(&q, &far)),
            5.0
        ));
        assert!(close(
            Distance::Manhattan.to_user(Distance::Manhattan.score(&q, &far)),
            7.0
        ));
    }

    #[test]
    fn serde_uses_lowercase_names() {
        assert_eq!(
            serde_json::to_string(&Distance::Euclid).unwrap(),
            "\"euclid\""
        );
        let d: Distance = serde_json::from_str("\"cosine\"").unwrap();
        assert_eq!(d, Distance::Cosine);
    }
}
