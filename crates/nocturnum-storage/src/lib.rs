//! Durable storage for one collection: the write-ahead log, segments, the generation manifest and record storage.

mod crash;
pub mod error;
mod sync;
pub mod wal;

pub use error::{Error, Result};
pub use wal::{Recovery, Wal, WalOptions};
