//! Exact search: score every candidate and keep the best `k`.
//!
//! This is the reference every approximate index is measured against, and the plan the query
//! layer falls back to when a filter leaves few enough candidates that a scan is cheaper than a
//! graph walk.
//!
//! Queries and stored vectors must already be in their prepared form, see
//! [`Distance::prepare`], which for cosine means unit length. Scores are the engine's internal
//! ones, where higher is better, and [`Distance::to_user`] converts them for the API.
//!
//! # Ordering
//!
//! Results are ordered by score, best first, and equal scores by key, smallest first, so a
//! search over the same data always returns the same list. A NaN score, which only arises
//! when a dot product overflows to infinities of both signs, ranks as negative infinity.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use nocturnum_core::Distance;

use crate::payload::Bounds;

/// A key with its score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scored<K> {
    pub key: K,
    pub score: f32,
}

/// Wraps a result so that a "greater" value is a worse one, which puts the worst kept result
/// on top of [`BinaryHeap`], ready to be evicted.
struct Worst<K>(Scored<K>);

impl<K: Ord> Ord for Worst<K> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Scores are never NaN here, see `rank`.
        other
            .0
            .score
            .partial_cmp(&self.0.score)
            .expect("ranked scores are never NaN")
            .then_with(|| self.0.key.cmp(&other.0.key))
    }
}

impl<K: Ord> PartialOrd for Worst<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: Ord> PartialEq for Worst<K> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<K: Ord> Eq for Worst<K> {}

#[inline]
fn rank(score: f32) -> f32 {
    if score.is_nan() {
        f32::NEG_INFINITY
    } else {
        score
    }
}

/// Keeps the best `k` results pushed into it, using O(k) memory whatever the number pushed.
pub struct TopK<K> {
    k: usize,
    heap: BinaryHeap<Worst<K>>,
}

impl<K: Ord> TopK<K> {
    pub fn new(k: usize) -> TopK<K> {
        TopK {
            k,
            // Capped so a huge `k` from a request does not allocate up front.
            heap: BinaryHeap::with_capacity(k.saturating_add(1).min(4096)),
        }
    }

    /// The score a new result has to beat to be kept, once `k` results are held.
    #[inline]
    pub fn threshold(&self) -> Option<f32> {
        if self.heap.len() < self.k {
            None
        } else {
            self.heap.peek().map(|w| w.0.score)
        }
    }

    /// Whether [`TopK::push`] would keep this result, which lets a caller skip expensive
    /// checks on results that would be dropped anyway.
    #[inline]
    pub fn would_keep(&self, key: &K, score: f32) -> bool {
        let score = rank(score);
        if self.k == 0 {
            return false;
        }
        if self.heap.len() < self.k {
            return true;
        }
        let worst = &self.heap.peek().expect("k is above zero").0;
        score > worst.score || (score == worst.score && *key < worst.key)
    }

    #[inline]
    pub fn push(&mut self, key: K, score: f32) {
        // Most candidates in a large scan lose here, before touching the heap.
        if !self.would_keep(&key, score) {
            return;
        }
        if self.heap.len() == self.k {
            self.heap.pop();
        }
        self.heap.push(Worst(Scored {
            key,
            score: rank(score),
        }));
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// The kept results, best first.
    pub fn into_sorted_vec(self) -> Vec<Scored<K>> {
        // Ascending by `Worst` is best first.
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|w| w.0)
            .collect()
    }
}

/// Scans vectors stored back to back in one slice, `dim` floats per row, and returns the best
/// `k` of the rows yielded by `rows`, best first.
///
/// Pass `0..row_count` to search everything, or only the rows a filter allows, such as a
/// sorted list from a payload index or `(0..n).filter(..)`. A row past the end of `vectors`
/// is a bug in the caller and panics.
pub fn search_rows(
    distance: Distance,
    vectors: &[f32],
    dim: usize,
    query: &[f32],
    k: usize,
    rows: impl IntoIterator<Item = u32>,
) -> Vec<Scored<u32>> {
    assert_eq!(query.len(), dim, "the query has the wrong dimension");
    assert!(dim > 0, "vectors have at least one dimension");
    assert_eq!(
        vectors.len() % dim,
        0,
        "vectors are not a whole number of rows"
    );
    let mut top = TopK::new(k);
    if k == 0 {
        return Vec::new();
    }
    for row in rows {
        let start = row as usize * dim;
        let v = &vectors[start..start + dim];
        top.push(row, distance.score(query, v));
    }
    top.into_sorted_vec()
}

/// Scans `(key, vector)` pairs from any source, such as an in-memory table, and returns the
/// best `k`, best first.
pub fn search<'a, K: Ord>(
    distance: Distance,
    query: &[f32],
    k: usize,
    points: impl IntoIterator<Item = (K, &'a [f32])>,
) -> Vec<Scored<K>> {
    let mut top = TopK::new(k);
    if k == 0 {
        return Vec::new();
    }
    for (key, v) in points {
        assert_eq!(v.len(), query.len(), "a vector has the wrong dimension");
        top.push(key, distance.score(query, v));
    }
    top.into_sorted_vec()
}

/// Like [`search_rows`] over the rows a filter may match, where rows outside `bounds.sure`
/// are only kept if `check` says they match, and `check` is only asked about rows that score
/// well enough to enter the top `k`, so most uncertain rows never have their payload read.
///
/// Callers restrict `bounds` to the live rows first, see [`Bounds::restrict`].
pub fn search_filtered<E>(
    distance: Distance,
    vectors: &[f32],
    dim: usize,
    query: &[f32],
    k: usize,
    bounds: &Bounds,
    mut check: impl FnMut(u32) -> Result<bool, E>,
) -> Result<Vec<Scored<u32>>, E> {
    assert_eq!(query.len(), dim, "the query has the wrong dimension");
    assert!(dim > 0, "vectors have at least one dimension");
    let mut top = TopK::new(k);
    if k == 0 {
        return Ok(Vec::new());
    }
    for row in &bounds.maybe {
        let start = row as usize * dim;
        let score = distance.score(query, &vectors[start..start + dim]);
        if top.would_keep(&row, score) && (bounds.sure.contains(row) || check(row)?) {
            top.push(row, score);
        }
    }
    Ok(top.into_sorted_vec())
}

#[cfg(test)]
mod tests;
