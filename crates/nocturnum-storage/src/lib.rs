//! Durable storage for one collection: the write-ahead log, segments, the generation manifest and record storage.

mod crash;
pub mod error;
pub mod manifest;
pub mod op;
pub mod optimizer;
mod segment;
pub mod shard;
mod sync;
pub mod wal;

pub use error::{Error, Result};
pub use op::{Operation, PointWrite};
pub use optimizer::Optimizer;
pub use segment::GraphOptions;
pub use shard::{
    Job, OptimizerOptions, OptimizerStatus, PointRef, Record, SegmentView, Shard, ShardOptions,
    ShardRecovery, View, WriteError,
};
pub use wal::{Recovery, Wal, WalOptions};
