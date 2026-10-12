//! Fusing several ranked lists of points into one.
//!
//! A hybrid query runs several searches, such as one by vector and one by words, whose
//! scores mean different things, and fusion turns their lists into one ranking. Every list
//! comes best first with the engine's internal scores, where higher is always better, so a
//! distance metric needs no special case, and carries a weight, 1 unless a query sets it.
//!
//! # Reciprocal rank fusion
//!
//! Each list adds `weight / (k + rank)` to every point in it, with ranks counted from 1, so
//! only positions matter and the scores themselves are ignored, while `k` (60 by default)
//! flattens the difference between the top places as it grows.
//!
//! # Distribution-based score fusion
//!
//! Each list's scores are rescaled by its own mean `m` and sample standard deviation `s`, so
//! that `m - 3s` maps to 0 and `m + 3s` to 1, clamped to that range, and each point adds
//! `weight` times its rescaled score. A list with one point, or whose scores are all equal,
//! gives each of them 0.5, and a score that overflowed takes 1 or 0 by its sign.
//!
//! In both, a point missing from a list gets nothing from it, the fused scores are summed in
//! list order, and equal fused scores are ordered by id.

use std::collections::HashMap;

use nocturnum_core::PointId;
use nocturnum_index::Scored;

/// The `k` reciprocal rank fusion uses when a query gives none.
pub const DEFAULT_RRF_K: f64 = 60.0;

/// The largest `k` and the largest weight a query may give.
pub const MAX_FUSION_PARAM: f64 = 1e6;

/// How the lists are fused.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fusion {
    Rrf { k: f64 },
    Dbsf,
}

impl Fusion {
    /// Checks the fusion's own setting, returning what is wrong.
    pub fn validate(&self) -> Result<(), String> {
        if let Fusion::Rrf { k } = self
            && !(0.0..=MAX_FUSION_PARAM).contains(k)
        {
            return Err(format!("k must be between 0 and 1000000, got {k}"));
        }
        Ok(())
    }

    /// Fuses `lists`, each best first and given with its weight, into one list best first.
    pub fn fuse(&self, lists: &[(f64, Vec<Scored<PointId>>)]) -> Vec<Scored<PointId>> {
        let mut sums: HashMap<PointId, f64> = HashMap::new();
        for (w, list) in lists {
            match self {
                Fusion::Rrf { k } => {
                    for (rank, hit) in list.iter().enumerate() {
                        *sums.entry(hit.key).or_default() += w / (k + (rank + 1) as f64);
                    }
                }
                Fusion::Dbsf => {
                    for (hit, norm) in list.iter().zip(dbsf_normalise(list)) {
                        *sums.entry(hit.key).or_default() += w * norm;
                    }
                }
            }
        }
        // Ranked on the full sums, so points closer than an f32 can show keep their order.
        let mut out: Vec<(PointId, f64)> = sums.into_iter().collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out.into_iter()
            .map(|(key, score)| Scored {
                key,
                score: score as f32,
            })
            .collect()
    }
}

/// Checks a list's weight, returning what is wrong.
pub fn check_weight(w: f64) -> Result<(), String> {
    if !(0.0..=MAX_FUSION_PARAM).contains(&w) {
        return Err(format!("weight must be between 0 and 1000000, got {w}"));
    }
    Ok(())
}

/// A list's scores rescaled to 0 to 1 by the mean plus or minus three sample standard
/// deviations of its finite scores. A score that overflowed to infinity in either direction
/// takes 1 or 0, and a NaN, which ranks last, takes 0.
fn dbsf_normalise(list: &[Scored<PointId>]) -> Vec<f64> {
    let finite: Vec<f64> = list
        .iter()
        .map(|h| f64::from(h.score))
        .filter(|s| s.is_finite())
        .collect();
    let n = finite.len();
    let (mean, sd) = if n < 2 {
        (0.0, 0.0)
    } else {
        let mean = finite.iter().sum::<f64>() / n as f64;
        let var = finite.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
        (mean, var.sqrt())
    };
    list.iter()
        .map(|h| {
            let s = f64::from(h.score);
            if s.is_nan() {
                0.0
            } else if s.is_infinite() {
                if s > 0.0 { 1.0 } else { 0.0 }
            } else if sd > 0.0 && sd.is_finite() {
                (0.5 + (s - mean) / (6.0 * sd)).clamp(0.0, 1.0)
            } else {
                0.5
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[(u64, f32)]) -> Vec<Scored<PointId>> {
        items
            .iter()
            .map(|&(id, score)| Scored {
                key: PointId::Num(id),
                score,
            })
            .collect()
    }

    fn ids(fused: &[Scored<PointId>]) -> Vec<u64> {
        fused
            .iter()
            .map(|s| match s.key {
                PointId::Num(n) => n,
                _ => unreachable!(),
            })
            .collect()
    }

    const RRF: Fusion = Fusion::Rrf { k: DEFAULT_RRF_K };

    #[test]
    fn rrf_adds_reciprocal_ranks() {
        let a = list(&[(1, 0.9), (2, 0.8), (3, 0.1)]);
        let b = list(&[(3, 12.0), (1, 4.0)]);
        let fused = RRF.fuse(&[(1.0, a), (1.0, b)]);
        assert_eq!(ids(&fused), [1, 3, 2]);
        assert_eq!(fused[0].score, (1.0 / 61.0 + 1.0 / 62.0) as f32);
        assert_eq!(fused[1].score, (1.0 / 63.0 + 1.0 / 61.0) as f32);
        assert_eq!(fused[2].score, (1.0 / 62.0) as f32);
    }

    #[test]
    fn rrf_weights_scale_each_list() {
        let a = list(&[(1, 0.9), (2, 0.8)]);
        let b = list(&[(2, 1.0), (1, 0.5)]);
        let fused = Fusion::Rrf { k: 1.0 }.fuse(&[(1.0, a.clone()), (3.0, b.clone())]);
        assert_eq!(ids(&fused), [2, 1]);
        assert_eq!(fused[0].score, (1.0 / 3.0 + 3.0 / 2.0) as f32);
        // A weight of 0 leaves a list out of the scores while its points stay candidates.
        let fused = Fusion::Rrf { k: 1.0 }.fuse(&[(1.0, a), (0.0, b)]);
        assert_eq!(ids(&fused), [1, 2]);
    }

    #[test]
    fn dbsf_rescales_each_list_by_its_spread() {
        // Mean 2 and sample standard deviation 1: 1, 2 and 3 map to 1/3, 1/2 and 2/3.
        let a = list(&[(1, 3.0), (2, 2.0), (3, 1.0)]);
        let fused = Fusion::Dbsf.fuse(&[(1.0, a)]);
        assert_eq!(ids(&fused), [1, 2, 3]);
        let want = [2.0 / 3.0, 0.5, 1.0 / 3.0];
        for (s, w) in fused.iter().zip(want) {
            assert!((f64::from(s.score) - w).abs() < 1e-6);
        }
        // A single point and equal scores give 0.5, and an empty list gives nothing.
        let one = Fusion::Dbsf.fuse(&[(1.0, list(&[(9, -4.0)])), (1.0, vec![])]);
        assert_eq!(one[0].score, 0.5);
        let flat = Fusion::Dbsf.fuse(&[(2.0, list(&[(1, 7.0), (2, 7.0)]))]);
        assert_eq!((flat[0].score, flat[1].score), (1.0, 1.0));
        assert_eq!(ids(&flat), [1, 2]);
    }

    #[test]
    fn dbsf_clamps_outliers() {
        let mut items: Vec<(u64, f32)> = (0..20).map(|i| (i, 1.0)).collect();
        items.insert(0, (99, 1000.0));
        let fused = Fusion::Dbsf.fuse(&[(1.0, list(&items))]);
        assert_eq!(fused[0].score, 1.0);
        assert!(fused.iter().all(|s| (0.0..=1.0).contains(&s.score)));
    }

    #[test]
    fn dbsf_keeps_overflowed_scores_in_order() {
        let a = list(&[(1, 0.0), (2, -1.0), (3, f32::NEG_INFINITY), (4, f32::NAN)]);
        let fused = Fusion::Dbsf.fuse(&[(1.0, a)]);
        assert_eq!(ids(&fused), [1, 2, 3, 4]);
        assert_eq!((fused[2].score, fused[3].score), (0.0, 0.0));
        let b = list(&[(5, f32::INFINITY), (6, 1.0)]);
        let fused = Fusion::Dbsf.fuse(&[(1.0, b)]);
        assert_eq!(ids(&fused), [5, 6]);
        assert_eq!((fused[0].score, fused[1].score), (1.0, 0.5));
    }

    #[test]
    fn ranking_uses_the_full_sums() {
        // Both round to the same f32, the larger sum still ranks first.
        let a = list(&[(9, 1.0)]);
        let b = list(&[(1, 1.0)]);
        let fused = Fusion::Rrf { k: 60.0 }.fuse(&[(1.500_000_01, a), (1.5, b)]);
        assert_eq!(fused[0].score, fused[1].score);
        assert_eq!(ids(&fused), [9, 1]);
    }

    #[test]
    fn ties_go_to_the_smaller_id() {
        let fused = RRF.fuse(&[(1.0, list(&[(5, 1.0)])), (1.0, list(&[(2, 1.0)]))]);
        assert_eq!(ids(&fused), [2, 5]);
    }

    #[test]
    fn validation_names_the_problem() {
        assert!(RRF.validate().is_ok());
        assert!(
            Fusion::Rrf { k: -1.0 }
                .validate()
                .unwrap_err()
                .contains("k must")
        );
        assert!(Fusion::Rrf { k: f64::NAN }.validate().is_err());
        assert!(check_weight(2.0).is_ok());
        assert!(check_weight(f64::INFINITY).unwrap_err().contains("weight"));
    }
}
