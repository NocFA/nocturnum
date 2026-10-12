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
    Job, OptimizerOptions, OptimizerStatus, PointRef, Record, SegmentStatus, SegmentView, Shard,
    ShardOptions, ShardRecovery, ShardStatus, SnapshotInfo, View, WalStats, WriteError,
};
pub use sync::{DiskSpace, create_dir_all_durable, disk_space, sync_dir};
pub use wal::{Recovery, Wal, WalOptions};
