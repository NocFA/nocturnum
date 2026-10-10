use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::vector::Distance;

/// The largest vector dimension a collection accepts.
pub const MAX_DIM: usize = 65_536;

/// The vector layout of a collection: one dense vector per point in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorConfig {
    pub dim: usize,
    pub distance: Distance,
}

impl VectorConfig {
    pub fn validate(&self) -> Result<()> {
        if self.dim == 0 || self.dim > MAX_DIM {
            return Err(Error::InvalidConfig(format!(
                "dim must be between 1 and {MAX_DIM}, got {}",
                self.dim
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dim_bounds() {
        let ok = VectorConfig {
            dim: 768,
            distance: Distance::Cosine,
        };
        assert!(ok.validate().is_ok());
        assert!(VectorConfig { dim: 0, ..ok }.validate().is_err());
        assert!(
            VectorConfig {
                dim: MAX_DIM + 1,
                ..ok
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let r: Result<VectorConfig, _> =
            serde_json::from_str(r#"{"dim":4,"distance":"dot","size":4}"#);
        assert!(r.is_err());
    }
}
