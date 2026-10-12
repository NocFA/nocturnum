//! Measuring search quality: how many of the true nearest points graph search finds at a few
//! beam widths, against exact search, and how long each takes.
//!
//! Queries are the vectors of points sampled from the collection, so the numbers describe
//! searches that look like the stored data. A sampled point finds itself, which lifts recall a
//! little at small limits, and the console says so beside the result.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nocturnum_core::{Filter, PointId};

use super::{Collection, MAX_HNSW_EF, QueryError, Result, SearchRequest};
use crate::deadline::Deadline;

/// The most sampled queries one check runs.
pub const MAX_RECALL_SAMPLES: usize = 1000;

/// The largest `limit` a check measures recall at.
pub const MAX_RECALL_LIMIT: usize = 100;

/// The most beam widths one check compares.
pub const MAX_RECALL_EF_VALUES: usize = 16;

/// A check stops taking new samples once it has run this long, so one request cannot hold a
/// thread for minutes on a big collection.
pub const RECALL_TIME_BUDGET: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct RecallRequest {
    /// How many points to sample as queries.
    pub samples: usize,
    /// Recall is measured over the top `limit` results.
    pub limit: usize,
    /// The beam widths to compare, each at least `limit`.
    pub hnsw_ef: Vec<usize>,
    /// Applied to every search, exact and graph alike.
    pub filter: Option<Filter>,
    /// Picks the sample, the same one while the points and segments stay the same, and a
    /// fresh one each time when `None`.
    pub seed: Option<u64>,
}

/// Graph search at one beam width.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallRun {
    pub hnsw_ef: usize,
    /// The share of exact search's results that graph search also returned, over every
    /// sample, `None` when exact search found nothing at all.
    pub recall: Option<f64>,
    pub p50: Duration,
    pub p99: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecallReport {
    /// Queries run, fewer than asked when the collection holds fewer points or the check ran
    /// out of time, see [`RECALL_TIME_BUDGET`].
    pub samples: usize,
    pub limit: usize,
    /// Points in the collection when the sample was taken.
    pub points: usize,
    /// Of those, the points in segments with a graph, the rest are always scanned exactly.
    pub indexed_points: u64,
    pub exact_p50: Duration,
    pub exact_p99: Duration,
    /// One per beam width, smallest first.
    pub runs: Vec<RecallRun>,
}

impl RecallRequest {
    fn check(&self) -> Result<Vec<usize>> {
        if !(1..=MAX_RECALL_SAMPLES).contains(&self.samples) {
            return Err(QueryError::Invalid(format!(
                "samples must be between 1 and {MAX_RECALL_SAMPLES}, got {}",
                self.samples
            )));
        }
        if !(1..=MAX_RECALL_LIMIT).contains(&self.limit) {
            return Err(QueryError::Invalid(format!(
                "limit must be between 1 and {MAX_RECALL_LIMIT}, got {}",
                self.limit
            )));
        }
        if self.hnsw_ef.is_empty() || self.hnsw_ef.len() > MAX_RECALL_EF_VALUES {
            return Err(QueryError::Invalid(format!(
                "hnsw_ef takes 1 to {MAX_RECALL_EF_VALUES} values, got {}",
                self.hnsw_ef.len()
            )));
        }
        for &ef in &self.hnsw_ef {
            if !(self.limit..=MAX_HNSW_EF).contains(&ef) {
                return Err(QueryError::Invalid(format!(
                    "each hnsw_ef must be between limit ({}) and {MAX_HNSW_EF}, got {ef}",
                    self.limit
                )));
            }
        }
        if let Some(f) = &self.filter {
            f.validate()?;
        }
        let mut ef = self.hnsw_ef.clone();
        ef.sort_unstable();
        ef.dedup();
        Ok(ef)
    }
}

/// SplitMix64, enough to pick a fair sample without another dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`, close enough to uniform for `n` far below 2^64.
    fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next()) * u128::from(n)) >> 64) as u64
    }
}

/// Fisher and Yates' shuffle.
fn shuffle<T>(items: &mut [T], rng: &mut Rng) {
    for i in (1..items.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        items.swap(i, j);
    }
}

fn fresh_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// The value at quantile `q` of `times`, which it sorts, by the nearest-rank method.
fn quantile(times: &mut [Duration], q: f64) -> Duration {
    if times.is_empty() {
        return Duration::ZERO;
    }
    times.sort_unstable();
    let rank = (q * times.len() as f64).ceil() as usize;
    times[rank.clamp(1, times.len()) - 1]
}

impl Collection {
    /// Samples points as queries and compares graph search at each beam width with exact
    /// search, see the module docs.
    pub fn recall(&self, req: &RecallRequest) -> Result<RecallReport> {
        self.recall_within(req, RECALL_TIME_BUDGET)
    }

    fn recall_within(&self, req: &RecallRequest, budget: Duration) -> Result<RecallReport> {
        let widths = req.check()?;
        let mut rng = Rng(req.seed.unwrap_or_else(fresh_seed));
        let started = Instant::now();
        let out_of_time = || started.elapsed() >= budget;
        let timed_out = || {
            QueryError::TimedOut(format!(
                "drawing the sample took longer than {} seconds",
                budget.as_secs()
            ))
        };

        // Reservoir sampling, so the sample is uniform without collecting every id first.
        let (mut queries, points, indexed_points) = {
            let view = self.shard.view();
            let indexed: u64 = view
                .segments()
                .filter(|s| s.graph().is_some())
                .map(|s| s.live().len())
                .sum();
            let mut picked: Vec<Vec<f32>> = Vec::with_capacity(req.samples);
            for (seen, p) in view.points().enumerate() {
                // A scan that runs long gives up, so one request cannot hold a thread past
                // the budget.
                if seen % 4096 == 4095 && out_of_time() {
                    return Err(timed_out());
                }
                if picked.len() < req.samples {
                    picked.push(p.vector.to_vec());
                } else {
                    let j = rng.below(seen as u64 + 1) as usize;
                    if j < req.samples {
                        picked[j] = p.vector.to_vec();
                    }
                }
            }
            (picked, view.len(), indexed)
        };
        if out_of_time() {
            return Err(timed_out());
        }
        // The reservoir's slots are not in random order, so it is shuffled for a check cut
        // short by the budget to still run a fair sample.
        shuffle(&mut queries, &mut rng);

        let mut exact_times = Vec::with_capacity(queries.len());
        let mut found = vec![0u64; widths.len()];
        let mut times = vec![Vec::with_capacity(queries.len()); widths.len()];
        let mut expected = 0u64;
        let mut ran = 0;
        for q in &queries {
            // At least one sample runs, so a check that answers always measured something.
            if ran > 0 && out_of_time() {
                break;
            }
            ran += 1;
            // One view for the sample's searches, so a write in between cannot make exact
            // and graph search disagree.
            let view = self.shard.view();
            let search = |exact: bool, ef: Option<usize>| {
                let mut s = SearchRequest::new(q.clone(), req.limit);
                s.filter = req.filter.clone();
                s.exact = exact;
                s.hnsw_ef = ef;
                let start = Instant::now();
                let hits = self.search_in(&view, &s, Deadline::NONE)?;
                Ok::<_, QueryError>((
                    hits.into_iter().map(|h| h.id).collect::<Vec<PointId>>(),
                    start.elapsed(),
                ))
            };
            let (truth, took) = search(true, None)?;
            exact_times.push(took);
            expected += truth.len() as u64;
            let truth: HashSet<PointId> = truth.into_iter().collect();
            for (i, &ef) in widths.iter().enumerate() {
                let (hits, took) = search(false, Some(ef))?;
                times[i].push(took);
                found[i] += hits.iter().filter(|id| truth.contains(id)).count() as u64;
            }
        }

        let runs = widths
            .iter()
            .zip(found)
            .zip(times.iter_mut())
            .map(|((&hnsw_ef, found), t)| RecallRun {
                hnsw_ef,
                recall: (expected > 0).then(|| found as f64 / expected as f64),
                p50: quantile(t, 0.5),
                p99: quantile(t, 0.99),
            })
            .collect();
        Ok(RecallReport {
            samples: ran,
            limit: req.limit,
            points,
            indexed_points,
            exact_p50: quantile(&mut exact_times, 0.5),
            exact_p99: quantile(&mut exact_times, 0.99),
            runs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_use_the_nearest_rank() {
        let mut t: Vec<Duration> = (1..=100).rev().map(Duration::from_millis).collect();
        assert_eq!(quantile(&mut t, 0.5), Duration::from_millis(50));
        assert_eq!(quantile(&mut t, 0.99), Duration::from_millis(99));
        let mut one = vec![Duration::from_millis(7)];
        assert_eq!(quantile(&mut one, 0.99), Duration::from_millis(7));
        assert_eq!(quantile(&mut [], 0.5), Duration::ZERO);
    }

    #[test]
    fn a_shuffle_puts_every_item_first_about_equally_often() {
        // A check cut short runs a prefix of the sample, so every item has to be as likely
        // as any other to come early.
        let mut rng = Rng(3);
        let mut first = [0u32; 8];
        for _ in 0..80_000 {
            let mut items: Vec<usize> = (0..8).collect();
            shuffle(&mut items, &mut rng);
            first[items[0]] += 1;
        }
        for (item, n) in first.iter().enumerate() {
            assert!(
                (9_000..11_000).contains(n),
                "item {item} came first {n} times"
            );
        }
    }

    #[test]
    fn a_sample_drawn_past_the_budget_times_out() {
        let dir = tempfile::TempDir::new().unwrap();
        let (c, _) = Collection::open(
            dir.path(),
            nocturnum_storage::ShardOptions::default(),
            Some(nocturnum_core::VectorConfig {
                dim: 2,
                distance: nocturnum_core::Distance::Dot,
            }),
        )
        .unwrap();
        c.upsert(
            vec![nocturnum_storage::PointWrite {
                id: PointId::Num(1),
                vector: vec![1.0, 0.0],
                sparse: None,
                payload: Default::default(),
            }],
            nocturnum_core::Ack::Durable,
        )
        .unwrap();
        let req = RecallRequest {
            samples: 1,
            limit: 1,
            hnsw_ef: vec![1],
            filter: None,
            seed: Some(1),
        };
        assert!(matches!(
            c.recall_within(&req, Duration::ZERO),
            Err(QueryError::TimedOut(_))
        ));
        let r = c.recall_within(&req, Duration::from_secs(30)).unwrap();
        assert_eq!(r.samples, 1);
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = Rng(1);
        for n in [1u64, 2, 7, 1000] {
            for _ in 0..1000 {
                assert!(rng.below(n) < n);
            }
        }
    }
}
