//! Getting bytes onto stable storage.
//!
//! On Linux `fdatasync` is enough. On macOS `fsync` only hands data to the drive, which may
//! keep it in a volatile cache, so files are flushed with `F_FULLFSYNC`, which asks the drive
//! to write its cache out too.

use std::fs::{self, File};
use std::io;
use std::path::Path;

/// Flushes a file's data to stable storage.
///
/// On macOS a filesystem without `F_FULLFSYNC` (some network and FUSE mounts) gets an error
/// here rather than a plain fsync, because a plain fsync there does not make data durable.
pub(crate) fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor belongs to `file`, which outlives the call, and F_FULLFSYNC
        // takes no further arguments.
        let r = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        if r != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EINVAL)) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this filesystem does not support F_FULLFSYNC, so writes on it cannot be made \
                 durable, use a local APFS or HFS+ volume",
            ));
        }
        Err(err)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_data()
    }
}

/// Makes the creation, removal or renaming of entries in `dir` durable.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    sync_file(&File::open(dir)?)
}

/// The directory holding `path`, with `.` for a bare relative name.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Creates `dir` and any missing parents, then syncs every directory entry on the path so the
/// whole path survives a crash.
///
/// Entries that already existed are synced too, since another process may have created them
/// and stopped before syncing.
pub(crate) fn create_dir_all_durable(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    for entry in dir.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        if entry.parent().is_some() {
            // A parent this process may not even open, such as the filesystem root on some
            // systems, cannot hold a directory it created, so it is skipped.
            match sync_dir(parent_of(entry)) {
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
                other => other?,
            }
        }
    }
    sync_dir(dir)
}
