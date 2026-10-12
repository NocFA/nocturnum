//! Snapshots: a whole shard in one tar file, with a SHA-256 sidecar, restored through normal
//! recovery.
//!
//! A snapshot holds the manifest, every segment it lists and the WAL files. Taking one holds
//! the flush lock, so no flush, merge or payload index change can publish, delete a segment
//! or truncate the WAL meanwhile, while writes carry on into the WAL. The WAL files are listed
//! and the last applied write noted under the state lock, which writers need, so no rotation
//! can slip between them, and the WAL is synced up to that write, so the snapshot holds at
//! least every write acknowledged before it started. Since the WAL files are then copied as
//! they stand, it may hold some later writes too, possibly with a torn last record, which
//! recovery trims like after a crash. Either way it is a prefix of the shard's history.
//!
//! The tar is written to a staging file unique to the call, synced and hard-linked to its
//! name, which fails rather than replace an existing file, and the sidecar `<name>.sha256` is
//! written last, so a snapshot without a sidecar is unfinished.
//!
//! Restoring reserves the target by creating it empty, checks the file against its sidecar,
//! then extracts the same open file into a staging directory, streaming each entry and
//! accepting only the files a shard is made of. The staged shard is opened, which runs full
//! recovery and checks every segment, before it is renamed over the empty reservation, so the
//! target name only ever holds an empty directory or a shard that opens.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, Write};
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::*;
use crate::manifest::FILE_NAME as MANIFEST_FILE;
use crate::sync::{read_bounded, sync_file};

const INFO_FILE: &str = "nocturnum-snapshot.json";
const FORMAT: u32 = 1;
/// The info file is a few dozen bytes, anything much larger is not one.
const MAX_INFO_BYTES: u64 = 64 * 1024;
/// Staging files older than this belong to snapshots that died, so [`prune`] removes them.
const ABANDONED_AFTER: Duration = Duration::from_secs(3600);

/// What [`Shard::snapshot`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    pub bytes: u64,
    /// Lowercase hex, also in the sidecar file.
    pub sha256: String,
    /// Every write up to this sequence number is in the snapshot.
    pub seq: u64,
}

/// What [`prune`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    pub deleted: Vec<PathBuf>,
    /// Newer snapshots that failed their checksum, left in place and not counted as kept.
    pub invalid: Vec<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Info {
    format: u32,
    seq: u64,
    vectors: VectorConfig,
}

/// Hashes everything read through it.
struct HashingReader<R> {
    inner: R,
    hash: Sha256,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

/// Hashes everything written through it.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn sidecar(path: &Path) -> PathBuf {
    with_suffix(path, ".sha256")
}

/// File names of the staging files this process is writing now, which [`prune`] never
/// touches. Names are unique within the process, and comparing names rather than paths keeps
/// a relative path and its absolute or symlinked form from looking different.
static ACTIVE: Mutex<BTreeSet<std::ffi::OsString>> = Mutex::new(BTreeSet::new());

/// A staging name no other call, in this process or another, uses at the same time, of the
/// form `<path>.<pid>-<n>.partial`. It counts as active until the guard drops.
pub(super) fn staging(path: &Path) -> Staging {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = with_suffix(path, &format!(".{}-{n}.partial", std::process::id()));
    lock(&ACTIVE).insert(path.file_name().expect("a file name").to_owned());
    Staging(path)
}

pub(super) struct Staging(PathBuf);

impl std::ops::Deref for Staging {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Staging {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Some(name) = self.0.file_name() {
            lock(&ACTIVE).remove(name);
        }
    }
}

/// Whether the staging file `name` may still be written: this process has it open, or the
/// process named in it is alive.
fn staging_in_use(path: &Path) -> bool {
    if path.file_name().is_some_and(|n| lock(&ACTIVE).contains(n)) {
        return true;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let pid = name.as_deref().and_then(|n| {
        let stem = n.strip_suffix(".partial")?;
        let (_, tail) = stem.rsplit_once('.')?;
        tail.split_once('-')?.0.parse::<i32>().ok()
    });
    match pid {
        Some(pid) if pid == std::process::id() as i32 => false,
        Some(pid) => process_alive(pid),
        None => false,
    }
}

fn process_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks the process exists, it sends nothing.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn add_bytes<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, name, bytes)
        .ctx(|| format!("adding {name} to the snapshot"))
}

/// Hashes everything `f` holds from where it stands to its end.
fn hash_file(f: &mut File, path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .ctx(|| format!("reading {}", path.display()))?;
        if n == 0 {
            return Ok(hex(&hash.finalize()));
        }
        hash.update(&buf[..n]);
    }
}

fn expected_sha256(path: &Path) -> Result<String> {
    let side = sidecar(path);
    let bytes = match read_bounded(&side, 4096) {
        Ok(b) => b,
        // Far larger than a checksum, so not one.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            return Err(Error::Corrupt {
                path: side,
                offset: 0,
                reason: "the sidecar is too large to hold a SHA-256".into(),
            });
        }
        Err(e) => return Err(e).ctx(|| format!("reading {}", side.display())),
    };
    let text = String::from_utf8_lossy(&bytes);
    let sum = text.split_whitespace().next().unwrap_or("");
    if sum.len() != 64 || !sum.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Corrupt {
            path: side,
            offset: 0,
            reason: "the sidecar does not hold a SHA-256".into(),
        });
    }
    Ok(sum.to_ascii_lowercase())
}

/// The SHA-256 recorded in the sidecar of the snapshot at `path`, as lowercase hex, without
/// reading the snapshot itself.
pub fn snapshot_checksum(path: &Path) -> Result<String> {
    expected_sha256(path)
}

/// Deletes a snapshot and its sidecar, the sidecar first, so a snapshot left without one by a
/// crash part way is no longer listed, the same order [`prune`] uses.
pub fn remove_snapshot(path: &Path) -> Result<()> {
    fs::remove_file(sidecar(path)).ctx(|| format!("removing {}", path.display()))?;
    fs::remove_file(path).ctx(|| format!("removing {}", path.display()))?;
    let dir = parent_of(path);
    sync_dir(dir).ctx(|| format!("syncing {}", dir.display()))
}

enum CopyError {
    Read(io::Error),
    Write(Error),
}

/// Copies an archive entry into a file and returns the bytes copied, telling a failed read,
/// which means a damaged archive, from a failed write.
fn copy_entry(from: &mut impl Read, to: &mut File, out: &Path) -> Result<u64, CopyError> {
    let mut buf = vec![0; 1 << 16];
    let mut copied = 0;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(copied),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CopyError::Read(e)),
        };
        to.write_all(&buf[..n])
            .ctx(|| format!("writing {}", out.display()))
            .map_err(CopyError::Write)?;
        copied += n as u64;
    }
}

/// Whether the snapshot at `path` matches its sidecar.
/// Whether the snapshot at `path` matches its sidecar, a sidecar that holds no checksum
/// counting as a mismatch.
fn verify(path: &Path) -> Result<bool> {
    let expected = match expected_sha256(path) {
        Ok(s) => s,
        Err(Error::Corrupt { .. }) => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut f = File::open(path).ctx(|| format!("opening {}", path.display()))?;
    Ok(hash_file(&mut f, path)? == expected)
}

impl Shard {
    /// Writes a snapshot of the shard to `path`, which must not exist yet, and its sidecar to
    /// `<path>.sha256`.
    pub fn snapshot(&self, path: &Path) -> Result<SnapshotInfo> {
        if path.exists() {
            return Err(Error::Invalid(format!("{} already exists", path.display())));
        }
        let tmp = staging(path);
        let result = self.write_snapshot(&tmp, path);
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn write_snapshot(&self, tmp: &Path, path: &Path) -> Result<SnapshotInfo> {
        let manifest = lock(&self.flush);
        // After a failed manifest write the file on disk may not be the manifest in memory.
        self.check_flushable(&manifest)?;
        let (seq, wal_files) = {
            let state = read(&self.state);
            (
                state.last_applied,
                crate::wal::list_segments(&self.dir.join("wal"))?,
            )
        };
        self.wal.sync_to(seq)?;
        let manifest_path = self.dir.join(MANIFEST_FILE);
        let manifest_bytes =
            fs::read(&manifest_path).ctx(|| format!("reading {}", manifest_path.display()))?;
        let on_disk = Manifest::read(&self.dir)?;
        if on_disk
            .as_ref()
            .is_none_or(|m| m.generation != manifest.generation || m.segments != manifest.segments)
        {
            return Err(Error::Invalid(format!(
                "{}: the manifest on disk is not the one in use, restart before snapshotting",
                self.dir.display()
            )));
        }

        let file = File::create(tmp).ctx(|| format!("creating {}", tmp.display()))?;
        let mut tar = tar::Builder::new(Hashing {
            inner: BufWriter::with_capacity(1 << 20, file),
            hash: Sha256::new(),
            bytes: 0,
        });
        // Sparse entries would come out as a type restore refuses, files with holes included.
        tar.sparse(false);
        let info = Info {
            format: FORMAT,
            seq,
            vectors: self.config,
        };
        add_bytes(
            &mut tar,
            INFO_FILE,
            &serde_json::to_vec(&info).expect("info serialises"),
        )?;
        add_bytes(&mut tar, MANIFEST_FILE, &manifest_bytes)?;
        for id in &manifest.segments {
            let name = segment_dir_name(*id);
            let dir = self.dir.join("segments").join(&name);
            let mut files: Vec<_> = fs::read_dir(&dir)
                .ctx(|| format!("listing {}", dir.display()))?
                .map(|e| e.map(|e| e.path()))
                .collect::<io::Result<_>>()
                .ctx(|| format!("listing {}", dir.display()))?;
            files.sort();
            for file in files {
                let file_name = file.file_name().expect("a file").to_string_lossy();
                let in_tar = format!("segments/{name}/{file_name}");
                let mut f = File::open(&file).ctx(|| format!("opening {}", file.display()))?;
                tar.append_file(&in_tar, &mut f)
                    .ctx(|| format!("adding {} to the snapshot", file.display()))?;
            }
        }
        // WAL files grow while this runs, so each is read once and stored at the length read.
        for (_, file) in wal_files {
            let bytes = fs::read(&file).ctx(|| format!("reading {}", file.display()))?;
            let file_name = file.file_name().expect("a file").to_string_lossy();
            add_bytes(&mut tar, &format!("wal/{file_name}"), &bytes)?;
        }
        drop(manifest);

        let hashing = tar
            .into_inner()
            .ctx(|| format!("finishing {}", tmp.display()))?;
        let sha256 = hex(&hashing.hash.finalize());
        let bytes = hashing.bytes;
        let file = hashing
            .inner
            .into_inner()
            .map_err(|e| e.into_error())
            .ctx(|| format!("writing {}", tmp.display()))?;
        sync_file(&file).ctx(|| format!("syncing {}", tmp.display()))?;
        crash_point("snapshot.written_not_renamed", || {});
        // A hard link fails when the name exists, where a rename would replace it.
        fs::hard_link(tmp, path).ctx(|| format!("publishing {}", path.display()))?;
        fs::remove_file(tmp).ctx(|| format!("removing {}", tmp.display()))?;

        let file_name = path.file_name().expect("a file name").to_string_lossy();
        let side = sidecar(path);
        let side_tmp = staging(&side);
        let written = (|| {
            let mut f =
                File::create(&side_tmp).ctx(|| format!("creating {}", side_tmp.display()))?;
            f.write_all(format!("{sha256}  {file_name}\n").as_bytes())
                .ctx(|| format!("writing {}", side_tmp.display()))?;
            sync_file(&f).ctx(|| format!("syncing {}", side_tmp.display()))?;
            fs::rename(&side_tmp, &side).ctx(|| format!("renaming {}", side_tmp.display()))
        })();
        if written.is_err() {
            let _ = fs::remove_file(&side_tmp);
        }
        written?;
        let parent = parent_of(path);
        sync_dir(parent).ctx(|| format!("syncing {}", parent.display()))?;
        Ok(SnapshotInfo {
            path: path.to_owned(),
            bytes,
            sha256,
            seq,
        })
    }
}

/// Unpacks the snapshot at `path` into `dir`, which must not exist yet, after checking it
/// against its sidecar and opening the result. Open it with [`Shard::open`] afterwards.
pub fn restore(path: &Path, dir: &Path) -> Result<()> {
    let parent = parent_of(dir);
    create_dir_all_durable(parent).ctx(|| format!("creating {}", parent.display()))?;
    // Creating the target is the reservation, so two restores to one place cannot both run.
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(Error::Invalid(format!("{} already exists", dir.display())));
        }
        Err(e) => return Err(e).ctx(|| format!("creating {}", dir.display())),
    }
    let tmp = staging(dir);
    let result = unpack(path, &tmp, dir);
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
        // Only the empty reservation, a published shard is never removed.
        let _ = fs::remove_dir(dir);
    }
    result
}

fn unpack(path: &Path, tmp: &Path, dir: &Path) -> Result<()> {
    let corrupt = |reason: String| Error::Corrupt {
        path: path.to_owned(),
        offset: 0,
        reason,
    };
    let expected = expected_sha256(path)?;
    // One open file for checking and extracting, so the bytes checked are the bytes used.
    let mut f = File::open(path).ctx(|| format!("opening {}", path.display()))?;
    if hash_file(&mut f, path)? != expected {
        return Err(corrupt("the snapshot does not match its checksum".into()));
    }
    f.rewind().ctx(|| format!("reading {}", path.display()))?;

    fs::create_dir(tmp).ctx(|| format!("creating {}", tmp.display()))?;
    // The extraction is hashed too and checked before anything is published, since the file
    // could change between the two reads.
    let mut archive = tar::Archive::new(HashingReader {
        inner: f,
        hash: Sha256::new(),
    });
    let mut info = None;
    let mut manifest = false;
    let mut dirs = BTreeSet::new();
    // Raw entries keep long-name and extension records as entries of their own, which are
    // refused below, instead of buffering them whole.
    // The whole file was just read once to hash it, so a failure reading the archive now
    // means it is malformed, while failures writing the files out are the disk's.
    let damaged = |e: io::Error| corrupt(format!("the archive is damaged: {e}"));
    let entries = archive.entries().map_err(damaged)?;
    for entry in entries.raw(true) {
        let mut entry = entry.map_err(damaged)?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(corrupt(
                "the snapshot holds something other than files".into(),
            ));
        }
        let name = entry
            .path()
            .map_err(damaged)?
            .to_string_lossy()
            .into_owned();
        let target = allowed(&name).ok_or_else(|| corrupt(format!("unexpected file {name}")))?;
        if name == INFO_FILE {
            let mut bytes = Vec::new();
            (&mut entry)
                .take(MAX_INFO_BYTES)
                .read_to_end(&mut bytes)
                .map_err(damaged)?;
            let i: Info = serde_json::from_slice(&bytes)
                .map_err(|e| corrupt(format!("{INFO_FILE} does not parse: {e}")))?;
            if i.format != FORMAT {
                return Err(corrupt(format!(
                    "snapshot format {} is not supported",
                    i.format
                )));
            }
            info = Some(i);
            continue;
        }
        manifest |= name == MANIFEST_FILE;
        let out = tmp.join(&target);
        let parent = out
            .parent()
            .expect("files live in the shard directory")
            .to_owned();
        if dirs.insert(parent.clone()) {
            create_dir_all_durable(&parent).ctx(|| format!("creating {}", parent.display()))?;
        }
        let mut f = File::options()
            .write(true)
            .create_new(true)
            .open(&out)
            .ctx(|| format!("creating {}", out.display()))?;
        let size = entry.header().size().map_err(damaged)?;
        let copied = copy_entry(&mut entry, &mut f, &out).map_err(|e| match e {
            CopyError::Read(e) => damaged(e),
            CopyError::Write(e) => e,
        })?;
        if copied != size {
            return Err(corrupt(format!(
                "{name} holds {copied} of its {size} bytes, the archive ends early"
            )));
        }
        sync_file(&f).ctx(|| format!("syncing {}", out.display()))?;
    }
    let mut rest = archive.into_inner();
    io::copy(&mut rest, &mut io::sink()).map_err(damaged)?;
    if hex(&rest.hash.finalize()) != expected {
        return Err(corrupt("the snapshot changed while it was restored".into()));
    }
    let info = info.ok_or_else(|| corrupt(format!("the snapshot has no {INFO_FILE}")))?;
    if !manifest {
        return Err(corrupt(format!("the snapshot has no {MANIFEST_FILE}")));
    }
    for needed in ["segments", "wal"] {
        create_dir_all_durable(&tmp.join(needed)).ctx(|| format!("creating {}", tmp.display()))?;
    }
    for d in &dirs {
        sync_dir(d).ctx(|| format!("syncing {}", d.display()))?;
    }
    sync_dir(tmp).ctx(|| format!("syncing {}", tmp.display()))?;

    // Full recovery, which checks every segment and the WAL, before the shard gets its name.
    {
        let (shard, recovery) = Shard::open(tmp, ShardOptions::default(), None)
            .map_err(|e| corrupt(format!("the restored shard does not open: {e}")))?;
        if shard.config() != info.vectors || recovery.wal.last_seq < info.seq {
            return Err(corrupt(format!(
                "the snapshot promises {:?} up to write {}, it holds {:?} up to {}",
                info.vectors,
                info.seq,
                shard.config(),
                recovery.wal.last_seq
            )));
        }
    }
    sync_dir(tmp).ctx(|| format!("syncing {}", tmp.display()))?;
    // Renaming onto the empty reservation replaces it in one step.
    fs::rename(tmp, dir).ctx(|| format!("renaming {} to {}", tmp.display(), dir.display()))?;
    let parent = parent_of(dir);
    sync_dir(parent).ctx(|| format!("syncing {}", parent.display()))?;
    Ok(())
}

/// Where a file named `name` inside a snapshot goes, `None` for anything a shard is not made
/// of, which refuses absolute paths, `..` and every name outside the layout.
fn allowed(name: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = name.split('/').collect();
    let plain = |p: &str| {
        !p.is_empty() && p != "." && p != ".." && !p.contains('\\') && !p.starts_with('.')
    };
    let ok = match parts[..] {
        [n] => n == INFO_FILE || n == MANIFEST_FILE,
        ["wal", f] => plain(f) && f.ends_with(".wal"),
        ["segments", d, f] => plain(d) && plain(f) && d.starts_with("seg-"),
        _ => false,
    };
    ok.then(|| parts.iter().collect())
}

/// Keeps the newest `keep` snapshots in `dir` that match their checksums and deletes older
/// ones with their sidecars, newest first by name, so names must sort by age, as those from
/// [`snapshot_name`] do.
///
/// Checking reads each kept snapshot in full. One that fails is left in place, reported and
/// not counted, so a damaged newer snapshot never costs an older good one. Staging files are
/// deleted once they are an hour old and neither this process nor the process named in them
/// is still writing them.
pub fn prune(dir: &Path, keep: usize) -> Result<Pruned> {
    let mut snapshots = Vec::new();
    let mut out = Pruned::default();
    for entry in fs::read_dir(dir).ctx(|| format!("listing {}", dir.display()))? {
        let path = entry.ctx(|| format!("listing {}", dir.display()))?.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let Some(name) = name else { continue };
        if name.ends_with(".partial") {
            let age = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok());
            if age.is_some_and(|a| a > ABANDONED_AFTER) && !staging_in_use(&path) {
                fs::remove_file(&path).ctx(|| format!("removing {}", path.display()))?;
                out.deleted.push(path);
            }
        } else if name.ends_with(".snapshot") && sidecar(&path).exists() {
            snapshots.push(path);
        }
    }
    snapshots.sort();
    let mut kept = 0;
    for path in snapshots.into_iter().rev() {
        if kept < keep {
            if verify(&path)? {
                kept += 1;
            } else {
                out.invalid.push(path);
            }
            continue;
        }
        fs::remove_file(sidecar(&path)).ctx(|| format!("removing {}", path.display()))?;
        fs::remove_file(&path).ctx(|| format!("removing {}", path.display()))?;
        out.deleted.push(path);
    }
    if !out.deleted.is_empty() {
        sync_dir(dir).ctx(|| format!("syncing {}", dir.display()))?;
    }
    Ok(out)
}

/// A snapshot file name that sorts by age: the prefix, milliseconds since 1970 padded to 15
/// digits, and `.snapshot`.
pub fn snapshot_name(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{prefix}-{millis:015}.snapshot")
}
