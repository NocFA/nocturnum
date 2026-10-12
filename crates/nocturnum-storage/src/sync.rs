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

/// Makes the creation, removal or renaming of entries in `dir` durable, with `F_FULLFSYNC`
/// on macOS.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
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
/// and stopped before syncing, but a parent this process may not open, such as the
/// filesystem root on some systems, is skipped there, while every parent of an entry this
/// call creates has to sync, and is checked before anything is created, so a call that fails
/// leaves nothing behind for a retry to take as already there.
pub fn create_dir_all_durable(dir: &Path) -> io::Result<()> {
    // A relative path ends in an empty ancestor, which names no entry.
    let entries: Vec<(&Path, bool)> = dir
        .ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| (p, matches!(p.try_exists(), Ok(true))))
        .collect();
    // The topmost missing entry is the only one made in a directory this call did not make.
    if let Some((first_missing, _)) = entries.iter().rev().find(|(_, existed)| !existed) {
        File::open(parent_of(first_missing))?;
    }
    fs::create_dir_all(dir)?;
    for (entry, existed) in entries {
        if entry.parent().is_some() {
            match sync_dir(parent_of(entry)) {
                Err(e) if existed && e.kind() == io::ErrorKind::PermissionDenied => {}
                other => other?,
            }
        }
    }
    sync_dir(dir)
}

/// Reads a small file whole, refusing one larger than `max` bytes before allocating for it,
/// so a damaged or hostile metadata file cannot exhaust memory.
pub(crate) fn read_bounded(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is {len} bytes, more than the {max} it may be",
                path.display()
            ),
        ));
    }
    let mut out = Vec::with_capacity(len as usize);
    f.by_ref().take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} grew past {max} bytes while it was read", path.display()),
        ));
    }
    Ok(out)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_new_directory_in_an_unreadable_parent_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = dir.path().join("write-only");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o333)).unwrap();
        let made = create_dir_all_durable(&parent.join("new/deeper"));
        let again = create_dir_all_durable(&parent.join("new/deeper"));
        let left = parent.join("new").exists();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(made.is_err(), "its entry could not be synced");
        assert!(again.is_err(), "nor on a retry");
        assert!(!left, "nothing was created");
        // One that someone else made is synced as far as allowed.
        fs::create_dir(parent.join("theirs")).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o333)).unwrap();
        let theirs = create_dir_all_durable(&parent.join("theirs"));
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(theirs.is_ok());
        // The same through a relative path.
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o333)).unwrap();
        // Up to the root, then down to the same directory.
        let up = std::env::current_dir().unwrap().components().count() - 1;
        let mut relative: std::path::PathBuf = std::iter::repeat_n("..", up).collect();
        relative.push(parent.strip_prefix("/").unwrap());
        let made = create_dir_all_durable(&relative.join("rel/deeper"));
        let again = create_dir_all_durable(&relative.join("rel/deeper"));
        let left = parent.join("rel").exists();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(made.is_err() && again.is_err() && !left, "{relative:?}");
    }
}

/// The size of the disk holding a path and the bytes this process may still write there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub total: u64,
    pub available: u64,
}

/// How big the disk holding `path` is and how much of it is free for this process, when
/// the platform says.
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized out-param.
        let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        // SAFETY: statvfs returned 0, so it filled `st`.
        let st = unsafe { st.assume_init() };
        // The field types differ between platforms, so the casts are not always no-ops.
        #[allow(clippy::unnecessary_cast)]
        let frag = st.f_frsize as u64;
        #[allow(clippy::unnecessary_cast)]
        Some(DiskSpace {
            total: (st.f_blocks as u64).saturating_mul(frag),
            available: (st.f_bavail as u64).saturating_mul(frag),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}
