//! Choosing how to run a filtered search on one segment.
//!
//! The payload indexes give the rows a filter surely matches and the rows it may match, see
//! [`Bounds`], and the gap between them is settled by reading payloads. From that the planner
//! estimates how many rows match, as a minimum, an expected count and a maximum, and picks a
//! strategy:
//!
//! - **Enumerate** when few rows can match: score just those rows, which is exact and cheaper
//!   than walking a graph.
//! - **Graph** when many can: walk the segment's vector index and skip rows the filter rejects.
//!
//! The expected count only matters when the minimum and maximum fall on either side of the
//! threshold, and then it comes from reading a small sample of the uncertain rows, one from
//! each stretch of equal size.
//!
//! A segment without a graph, too small to have been given one, always runs the filtered flat
//! scan, [`nocturnum_index::flat::search_filtered`].

use nocturnum_core::Filter;
use nocturnum_index::{Bounds, PayloadIndex};
use roaring::RoaringBitmap;

/// How many rows a filter matches: at least `min`, at most `max`, and probably `expected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub min: u64,
    pub expected: u64,
    pub max: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Enumerate,
    Graph,
}

#[derive(Debug, Clone, Copy)]
pub struct PlanOptions {
    /// Enumerate when no more than this many rows are expected to match.
    pub enumerate_up_to: u64,
    /// How many uncertain rows to read when the estimate has to be sampled.
    pub samples: u64,
}

impl PlanOptions {
    /// Enumerates up to 10 MiB worth of vectors, which a flat scan gets through in a few
    /// milliseconds, see `docs/index/exact-search.md`.
    pub fn for_dim(dim: usize) -> PlanOptions {
        PlanOptions {
            enumerate_up_to: (10 * 1024 * 1024 / (dim.max(1) * 4)).max(1_000) as u64,
            samples: 64,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub bounds: Bounds,
    pub estimate: Estimate,
    pub strategy: Strategy,
}

/// Plans `filter` on one segment, over the rows in `live` when given, calling `check` to read
/// a sample of uncertain rows if the decision needs it.
pub fn plan<E>(
    index: &PayloadIndex,
    filter: &Filter,
    live: Option<&RoaringBitmap>,
    options: &PlanOptions,
    check: impl FnMut(u32) -> Result<bool, E>,
) -> Result<Plan, E> {
    let mut bounds = index.bounds(filter);
    if let Some(live) = live {
        bounds.restrict(live);
    }
    choose(bounds, options, check)
}

/// Plans from bounds already worked out, see [`plan`].
pub fn choose<E>(
    bounds: Bounds,
    options: &PlanOptions,
    check: impl FnMut(u32) -> Result<bool, E>,
) -> Result<Plan, E> {
    let (min, max) = (bounds.sure.len(), bounds.maybe.len());
    let (estimate, strategy) = if max <= options.enumerate_up_to {
        (unsampled(min, max), Strategy::Enumerate)
    } else if min > options.enumerate_up_to {
        (unsampled(min, max), Strategy::Graph)
    } else {
        let e = estimate(&bounds, options.samples, check)?;
        let s = if e.expected <= options.enumerate_up_to {
            Strategy::Enumerate
        } else {
            Strategy::Graph
        };
        (e, s)
    };
    Ok(Plan {
        bounds,
        estimate,
        strategy,
    })
}

/// The estimate without reading any payload, halfway between the bounds.
fn unsampled(min: u64, max: u64) -> Estimate {
    Estimate {
        min,
        expected: min + (max - min) / 2,
        max,
    }
}

/// Estimates the matching rows by checking up to `samples` uncertain rows, spread evenly, and
/// gives the same answer every time for the same rows.
pub fn estimate<E>(
    bounds: &Bounds,
    samples: u64,
    mut check: impl FnMut(u32) -> Result<bool, E>,
) -> Result<Estimate, E> {
    let (min, max) = (bounds.sure.len(), bounds.maybe.len());
    let uncertain = bounds.uncertain();
    let n = uncertain.len();
    let taken = samples.min(n);
    if taken == 0 {
        return Ok(unsampled(min, max));
    }
    let mut hits = 0u64;
    for i in 0..taken {
        // One row from each of `taken` equal strata, at a fixed pseudo-random offset inside
        // it, which spreads the sample like an even stride without lining up with data that
        // repeats at a fixed period.
        let (start, end) = (i * n / taken, (i + 1) * n / taken);
        let position = start + mix(i) % (end - start);
        let row = uncertain
            .select(position as u32)
            .expect("position is below the count");
        if check(row)? {
            hits += 1;
        }
    }
    let expected = min + (n as u128 * hits as u128 / taken as u128) as u64;
    Ok(Estimate { min, expected, max })
}

/// A fixed scramble of `i` (splitmix64's finaliser).
fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests;
