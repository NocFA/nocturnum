//! Durable storage for one collection: the write-ahead log, segments, the generation manifest and record storage.

mod crash;
pub mod error;
pub mod manifest;
pub mod op;
mod segment;
pub mod shard;
mod sync;
pub mod wal;

pub use error::{Error, Result};
pub use op::{Operation, PointWrite};
pub use shard::{
    PointRef, Record, SegmentView, Shard, ShardOptions, ShardRecovery, View, WriteError,
};
pub use wal::{Recovery, Wal, WalOptions};
