use thiserror::Error;

/// Errors from validating user input against the core types.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Error {
    #[error("vector has {got} dimensions, the collection expects {expected}")]
    DimensionMismatch { expected: usize, got: usize },

    #[error("vector contains a NaN or infinite value at position {index}")]
    NonFiniteVector { index: usize },

    #[error("a zero vector has no direction, so it cannot be used with cosine distance")]
    ZeroVector,

    #[error("vector is too long to score without overflowing, its length must stay below 6.5e18")]
    VectorTooLarge,

    #[error("invalid sparse vector: {0}")]
    InvalidSparse(String),

    #[error("invalid json path {path:?}: {reason}")]
    InvalidPath { path: String, reason: String },

    #[error("invalid filter at {at}: {reason}")]
    InvalidFilter { at: String, reason: String },

    #[error("payloads may nest at most {max} levels deep")]
    PayloadTooDeep { max: usize },

    #[error("invalid collection config: {0}")]
    InvalidConfig(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
