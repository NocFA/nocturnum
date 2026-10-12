//! Text search: ranking points by BM25 over a field with a text index.
//!
//! The corpus statistics come from the same view as the scores, counted over every live point
//! whatever the filter, so a filter narrows which points come back without changing how any
//! of them scores. Segments answer from their text indexes, and points not yet in a segment
//! are tokenized as the search runs, see `docs/decisions/0004-text-ranking.md`.

use std::collections::HashMap;

use nocturnum_core::filter::field_values;
use nocturnum_core::{Filter, JsonPath, PayloadIndexKind, PayloadIndexSpec, PointId};
use nocturnum_index::text::{document, idf, query_terms};
use nocturnum_index::{Scored, TextIndex, TopK};
use nocturnum_storage::{SegmentView, View};

use super::{QueryError, Result, TextQuery, row_matches, segment_bounds};

/// The most different words one text query may hold, which bounds the work and memory a
/// single search takes.
pub const MAX_QUERY_TERMS: usize = 1024;

/// Whether `field` has a text index in `view`.
pub(super) fn has_text_index(indexes: &[PayloadIndexSpec], field: &JsonPath) -> bool {
    indexes
        .iter()
        .any(|s| &s.path == field && s.kind == PayloadIndexKind::Text)
}

/// The best `window` points for `q` among those matching `filter`, best first.
pub(super) fn rank(
    view: &View,
    q: &TextQuery,
    filter: Option<&Filter>,
    window: usize,
) -> Result<Vec<Scored<PointId>>> {
    q.bm25.validate().map_err(QueryError::Invalid)?;
    if !has_text_index(view.payload_indexes(), &q.field) {
        return Err(QueryError::NoTextIndex(q.field.clone()));
    }
    let terms = query_terms(&q.query, MAX_QUERY_TERMS).ok_or(QueryError::TooManyWords)?;
    if terms.is_empty() || window == 0 {
        return Ok(Vec::new());
    }

    // Recent points: counted into the statistics, and kept for scoring when they match.
    let mut docs = 0u64;
    let mut total_len = 0u64;
    let mut df = vec![0u64; terms.len()];
    let mut recent = Vec::new();
    for p in view.recent() {
        let payload = p.payload()?;
        let (counts, len) = document(&field_values(&q.field, &payload.0));
        if len == 0 {
            continue;
        }
        docs += 1;
        total_len += u64::from(len);
        // Only the terms the document holds, in the order of `terms`.
        let tfs: Vec<(usize, u32)> = terms
            .iter()
            .enumerate()
            .filter_map(|(i, t)| counts.get(t).map(|&tf| (i, tf)))
            .collect();
        for &(i, _) in &tfs {
            df[i] += 1;
        }
        if !tfs.is_empty() && filter.is_none_or(|f| f.matches(p.id, &payload)) {
            recent.push((p.id, len, tfs));
        }
    }

    let segments: Vec<(SegmentView<'_>, &TextIndex)> = view
        .segments()
        .map(|seg| {
            let text = seg
                .payload_index()
                .text(&q.field)
                .expect("every segment builds the indexes its view lists");
            (seg, text)
        })
        .collect();
    for (seg, text) in &segments {
        let (d, len) = text.stats(seg.live());
        docs += d;
        total_len += len;
        for (n, term) in df.iter_mut().zip(&terms) {
            *n += text.df(term, seg.live());
        }
    }
    if docs == 0 {
        return Ok(Vec::new());
    }
    let avg_len = total_len as f64 / docs as f64;
    let idfs: Vec<f64> = df.iter().map(|&n| idf(docs, n)).collect();

    // Every document adds its terms' shares in the order of `terms`, wherever it lives, so a
    // point scores the same before and after a flush.
    let mut top = TopK::new(window);
    for (id, len, tfs) in recent {
        let mut score = 0.0f64;
        for (i, tf) in tfs {
            score += q.bm25.term(idfs[i], tf, len, avg_len);
        }
        top.push(id, score as f32);
    }
    for (seg, text) in &segments {
        let live = seg.live();
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for (term, &idf) in terms.iter().zip(&idfs) {
            let Some(p) = text.postings(term) else {
                continue;
            };
            for (&row, &tf) in p.rows.iter().zip(&p.tfs) {
                if live.contains(row) {
                    *scores.entry(row).or_default() +=
                        q.bm25.term(idf, tf, text.len_of(row), avg_len);
                }
            }
        }
        let bounds = filter.map(|f| (f, segment_bounds(seg, Some(f))));
        for (row, score) in scores {
            let id = seg.point(row).id;
            let score = score as f32;
            if !top.would_keep(&id, score) {
                continue;
            }
            let keep = match &bounds {
                None => true,
                Some((_, b)) if b.sure.contains(row) => true,
                Some((f, b)) => b.maybe.contains(row) && row_matches(seg, f, row)?,
            };
            if keep {
                top.push(id, score);
            }
        }
    }
    Ok(top.into_sorted_vec())
}

#[cfg(test)]
mod tests;
