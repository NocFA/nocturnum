//! Sparse search: ranking points by the dot product of their sparse vector with a query's.
//!
//! Only points sharing at least one dimension with the query are results. With `idf` each
//! query weight is first multiplied by how rare its dimension is, counted over the live points
//! with a sparse vector when the search runs, whatever the filter, as text search does.

use std::collections::HashMap;

use crate::deadline::Deadline;
use nocturnum_core::{Filter, PointId, SparseVector};
use nocturnum_index::text::idf;
use nocturnum_index::{Scored, TopK};
use nocturnum_storage::View;

use super::{Result, row_matches, segment_bounds};

/// A sparse search.
#[derive(Debug, Clone, PartialEq)]
pub struct SparseQuery {
    pub vector: SparseVector,
    /// Weighs each dimension by its inverse document frequency.
    pub idf: bool,
}

/// The best `window` points for `q` among those matching `filter`, best first.
pub(super) fn rank(
    view: &View,
    q: &SparseQuery,
    filter: Option<&Filter>,
    window: usize,
    deadline: Deadline,
) -> Result<Vec<Scored<PointId>>> {
    let Some(query) = q.vector.clone().prepare()? else {
        return Ok(Vec::new());
    };
    if window == 0 {
        return Ok(Vec::new());
    }

    // Recent points holding a query dimension, with their weights in query order.
    let mut docs = 0u64;
    let mut df = vec![0u64; query.len()];
    let mut recent = Vec::new();
    for (i, p) in view.recent().enumerate() {
        deadline.tick(i)?;
        let Some(v) = p.sparse() else { continue };
        docs += 1;
        let shared: Vec<(usize, f32)> = query
            .indices
            .iter()
            .enumerate()
            .filter_map(|(i, &d)| v.get(d).map(|w| (i, w)))
            .collect();
        for &(i, _) in &shared {
            df[i] += 1;
        }
        if !shared.is_empty()
            && filter.map_or(Ok(true), |f| p.payload().map(|pl| f.matches(p.id, &pl)))?
        {
            recent.push((p.id, shared));
        }
    }
    if q.idf {
        for seg in view.segments() {
            deadline.check()?;
            let ix = seg.sparse_index();
            docs += ix.count(seg.live());
            for (n, &d) in df.iter_mut().zip(&query.indices) {
                *n += ix.df(d, seg.live());
            }
        }
    }
    // The weight each query dimension carries, after IDF when asked for.
    let weights: Vec<f64> = query
        .values
        .iter()
        .zip(&df)
        .map(|(&w, &n)| {
            let w = f64::from(w);
            if q.idf { w * idf(docs, n) } else { w }
        })
        .collect();

    // Every point adds its shared dimensions in query order, wherever it lives, so a point
    // scores the same before and after a flush.
    let mut top = TopK::new(window);
    for (id, shared) in recent {
        let score: f64 = shared
            .iter()
            .fold(0.0, |acc, &(i, w)| acc + weights[i] * f64::from(w));
        top.push(id, score as f32);
    }
    for seg in view.segments() {
        deadline.check()?;
        let ix = seg.sparse_index();
        let live = seg.live();
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for (&d, &qw) in query.indices.iter().zip(&weights) {
            let Some(p) = ix.postings(d) else { continue };
            for (n, (&row, &w)) in p.rows.iter().zip(&p.weights).enumerate() {
                deadline.tick(n)?;
                if live.contains(row) {
                    *scores.entry(row).or_default() += qw * f64::from(w);
                }
            }
        }
        let bounds = filter.map(|f| (f, segment_bounds(&seg, Some(f))));
        for (n, (row, score)) in scores.into_iter().enumerate() {
            deadline.tick(n)?;
            let id = seg.point(row).id;
            let score = score as f32;
            if !top.would_keep(&id, score) {
                continue;
            }
            let keep = match &bounds {
                None => true,
                Some((_, b)) if b.sure.contains(row) => true,
                Some((f, b)) => b.maybe.contains(row) && row_matches(&seg, f, row)?,
            };
            if keep {
                top.push(id, score);
            }
        }
    }
    Ok(top.into_sorted_vec())
}

impl From<SparseVector> for SparseQuery {
    fn from(vector: SparseVector) -> Self {
        SparseQuery { vector, idf: false }
    }
}

#[cfg(test)]
mod tests;
