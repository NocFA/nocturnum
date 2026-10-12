//! Sparse vectors: a few weighted dimensions out of a space too large to store densely, such
//! as the vocabulary of a learned sparse model like SPLADE.
//!
//! A sparse vector is stored with its dimensions in increasing order, each at most once and
//! each weight finite, and an empty one is the same as none. Two sparse vectors score by the
//! dot product of the dimensions they share.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The most dimensions one sparse vector may hold.
pub const MAX_SPARSE_LEN: usize = 65_536;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

impl SparseVector {
    /// Checks the vector and returns the form the engine stores, sorted by dimension, or
    /// `None` when it has no dimensions.
    pub fn prepare(self) -> Result<Option<SparseVector>> {
        let SparseVector { indices, values } = self;
        let bad = |reason: String| Err(Error::InvalidSparse(reason));
        if indices.len() != values.len() {
            return bad(format!(
                "indices has {} entries and values {}, they must match",
                indices.len(),
                values.len()
            ));
        }
        if indices.len() > MAX_SPARSE_LEN {
            return bad(format!(
                "a sparse vector holds at most {MAX_SPARSE_LEN} dimensions, got {}",
                indices.len()
            ));
        }
        if let Some(i) = values.iter().position(|v| !v.is_finite()) {
            return bad(format!("values[{i}] is a NaN or infinite"));
        }
        if indices.is_empty() {
            return Ok(None);
        }
        let mut pairs: Vec<(u32, f32)> = indices.into_iter().zip(values).collect();
        pairs.sort_by_key(|p| p.0);
        if let Some(w) = pairs.windows(2).find(|w| w[0].0 == w[1].0) {
            return bad(format!("dimension {} appears more than once", w[0].0));
        }
        Ok(Some(SparseVector {
            indices: pairs.iter().map(|p| p.0).collect(),
            values: pairs.iter().map(|p| p.1).collect(),
        }))
    }

    /// The weight of dimension `index`, if the vector holds it. The vector must be prepared.
    pub fn get(&self, index: u32) -> Option<f32> {
        self.indices
            .binary_search(&index)
            .ok()
            .map(|i| self.values[i])
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
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
    fn prepare_sorts_and_checks() {
        let v = sv(&[9, 2, 5], &[0.9, 0.2, 0.5]).prepare().unwrap().unwrap();
        assert_eq!(v, sv(&[2, 5, 9], &[0.2, 0.5, 0.9]));
        assert_eq!(v.get(5), Some(0.5));
        assert_eq!(v.get(4), None);
        assert_eq!(sv(&[], &[]).prepare().unwrap(), None);
        for bad in [
            sv(&[1, 2], &[0.1]),
            sv(&[1, 1], &[0.1, 0.2]),
            sv(&[1], &[f32::NAN]),
            sv(&[1], &[f32::INFINITY]),
        ] {
            assert!(matches!(bad.prepare(), Err(Error::InvalidSparse(_))));
        }
        let long: Vec<u32> = (0..=MAX_SPARSE_LEN as u32).collect();
        let weights = vec![1.0; long.len()];
        assert!(sv(&long, &weights).prepare().is_err());
    }

    #[test]
    fn serde_shape() {
        let v: SparseVector =
            serde_json::from_str(r#"{"indices": [1, 7], "values": [0.5, -1]}"#).unwrap();
        assert_eq!(v, sv(&[1, 7], &[0.5, -1.0]));
        assert!(
            serde_json::from_str::<SparseVector>(r#"{"indices": [], "values": [], "x": 1}"#)
                .is_err()
        );
    }
}
