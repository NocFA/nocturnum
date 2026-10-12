//! Inverted indexes over sparse vectors, one per immutable segment.
//!
//! Each dimension keeps the rows whose sparse vector holds it with their weights, so scoring
//! a query visits only the rows sharing a dimension with it. Like the payload indexes the
//! index is derived data, built in memory from the segment when it is opened or written.

use std::collections::HashMap;

use nocturnum_core::SparseVector;
use roaring::RoaringBitmap;

/// The rows holding one dimension, in increasing order, with their weights.
#[derive(Debug, Default)]
pub struct SparsePostings {
    pub rows: Vec<u32>,
    pub weights: Vec<f32>,
}

/// One segment's sparse vectors, inverted.
#[derive(Debug, Default)]
pub struct SparseIndex {
    dims: HashMap<u32, SparsePostings>,
    /// Rows with a sparse vector.
    rows: RoaringBitmap,
}

impl SparseIndex {
    /// Builds the index from each row's sparse vector, rows in increasing order.
    pub fn build<'a>(vectors: impl IntoIterator<Item = (u32, &'a SparseVector)>) -> SparseIndex {
        let mut ix = SparseIndex::default();
        for (row, v) in vectors {
            ix.add(row, v);
        }
        ix
    }

    /// Adds a row's sparse vector, rows coming in increasing order.
    pub fn add(&mut self, row: u32, v: &SparseVector) {
        if v.is_empty() {
            return;
        }
        self.rows.insert(row);
        for (&d, &w) in v.indices.iter().zip(&v.values) {
            let p = self.dims.entry(d).or_default();
            debug_assert!(p.rows.last().is_none_or(|&r| r < row), "rows in order");
            p.rows.push(row);
            p.weights.push(w);
        }
    }

    /// The rows holding dimension `dim`.
    pub fn postings(&self, dim: u32) -> Option<&SparsePostings> {
        self.dims.get(&dim)
    }

    /// How many rows among `live` have a sparse vector.
    pub fn count(&self, live: &RoaringBitmap) -> u64 {
        self.rows.intersection_len(live)
    }

    /// How many rows among `live` hold dimension `dim`.
    pub fn df(&self, dim: u32, live: &RoaringBitmap) -> u64 {
        self.postings(dim).map_or(0, |p| {
            p.rows.iter().filter(|r| live.contains(**r)).count() as u64
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(indices: &[u32], values: &[f32]) -> SparseVector {
        SparseVector {
            indices: indices.to_vec(),
            values: values.to_vec(),
        }
    }

    #[test]
    fn postings_and_counts_follow_live_rows() {
        let a = sv(&[1, 5], &[0.5, 2.0]);
        let b = sv(&[5], &[1.0]);
        let c = sv(&[], &[]);
        let ix = SparseIndex::build([(0, &a), (2, &b), (3, &c)]);
        let p = ix.postings(5).unwrap();
        assert_eq!(
            (p.rows.as_slice(), p.weights.as_slice()),
            (&[0, 2][..], &[2.0, 1.0][..])
        );
        assert!(ix.postings(9).is_none());
        let mut live = RoaringBitmap::new();
        live.insert_range(0..4);
        assert_eq!(
            (ix.count(&live), ix.df(5, &live), ix.df(1, &live)),
            (2, 2, 1)
        );
        live.remove(0);
        assert_eq!(
            (ix.count(&live), ix.df(5, &live), ix.df(1, &live)),
            (1, 1, 0)
        );
    }
}
