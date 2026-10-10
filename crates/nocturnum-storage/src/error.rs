use std::io;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    #[error("{} is corrupt at byte {offset}: {reason}", path.display())]
    Corrupt {
        path: PathBuf,
        offset: u64,
        reason: String,
    },

    #[error("a record of {size} bytes is over the {max} byte limit")]
    RecordTooLarge { size: usize, max: u32 },

    #[error(
        "the wal stopped accepting writes after an earlier write or fsync failed, \
         restart the server to recover from what is on disk"
    )]
    Poisoned,

    #[error("{0}")]
    Invalid(String),

    #[error("{} is already open in another process or handle", dir.display())]
    Locked { dir: PathBuf },

    #[error("sequence {seq} has not been written yet, the last written is {last}")]
    NotWritten { seq: u64, last: u64 },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) trait IoContext<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: context(),
            source,
        })
    }
}
