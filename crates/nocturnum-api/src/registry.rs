//! The collections on disk and their snapshots.
//!
//! Layout under the data directory:
//!
//! ```text
//! LOCK                           held while the registry is open
//! schedules.json                 snapshot schedules, see `crate::schedule`
//! collections/<name>/            one shard per collection
//! snapshots/<name>/<file>.snapshot and <file>.snapshot.sha256
//! ```
//!
//! The snapshots can live elsewhere, set by [`RegistryOptions::snapshots_dir`], with a `LOCK`
//! of their own so two servers never prune each other's.
//!
//! Collection names never start with a dot, so the registry keeps its work in progress in
//! dot-named directories beside them:
//!
//! - `.creating-<uuid>`: a new collection, renamed to its name once it exists in full.
//! - `.deleted-<uuid>`: a deleted collection, renamed away before it is removed.
//! - `.restoring-<name>`: a snapshot being unpacked for `<name>`.
//! - `.replaced-<name>`: the collection a restore is replacing, moved aside only once the
//!   restored copy is complete and on disk.
//!
//! Startup finishes a restore that had moved the old copy aside, since the new one was
//! complete by then, puts the old copy back if the new one is missing, and removes every
//! other dot-named entry.
//!
//! Every method blocks on disk and may wait for requests in flight, so call them from
//! blocking threads.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use nocturnum_core::VectorConfig;
use nocturnum_query::Collection;
use nocturnum_storage::shard::{prune, remove_snapshot, restore, snapshot_checksum};
use nocturnum_storage::{OptimizerOptions, ShardOptions, create_dir_all_durable, sync_dir};

use crate::cron::civil;
use crate::error::ApiError;
use crate::schedule::{Schedule, ScheduleState, Schedules};

/// The longest collection name.
pub const MAX_NAME_LEN: usize = 63;

#[derive(Debug, Clone, Default)]
pub struct RegistryOptions {
    pub shard: ShardOptions,
    /// Runs an optimizer for every collection, or none when `None`.
    pub optimizer: Option<OptimizerOptions>,
    /// Where snapshots are kept, `snapshots` in the data directory when `None`.
    pub snapshots_dir: Option<PathBuf>,
}

/// A snapshot file, as the API describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SnapshotEntry {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    /// RFC 3339 in UTC.
    pub created_at: String,
}

pub struct Registry {
    collections_dir: PathBuf,
    snapshots_dir: PathBuf,
    options: RegistryOptions,
    open: RwLock<BTreeMap<String, Arc<Collection>>>,
    /// Held by create, delete and restore, so they take turns.
    changes: Mutex<()>,
    /// The time in the newest snapshot name, in milliseconds, kept rising so two snapshots
    /// taken in the same millisecond still get different names.
    last_snapshot_ms: Mutex<u64>,
    /// Collections on disk that could not be put back in service after a failed delete or
    /// restore, by name, which keep `/readyz` failing until a restart.
    lost: Mutex<BTreeMap<String, String>>,
    schedules: Schedules,
    /// Held while scheduled runs prune and while schedules are removed, see
    /// `Registry::run_schedule`.
    retention: Mutex<()>,
    /// The exclusive locks on the data and snapshot directories, so a second server cannot
    /// tidy away this one's work in progress or prune its snapshots.
    _locks: [fs::File; 2],
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Checks a collection name: 1 to 63 of `a-z`, `0-9`, `_` and `-`, starting with a letter
/// or digit.
pub fn check_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric();
    if ok {
        Ok(())
    } else {
        Err(ApiError::invalid(format!(
            "invalid collection name {name:?}: use 1 to {MAX_NAME_LEN} of a-z, 0-9, _ and -, \
             starting with a letter or digit"
        )))
    }
}

/// Checks a snapshot file name, which must be a plain file name ending in `.snapshot`.
fn check_snapshot_name(name: &str) -> Result<(), ApiError> {
    let ok = name.len() <= 255
        && name.ends_with(".snapshot")
        && !name.starts_with('.')
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        });
    if ok {
        Ok(())
    } else {
        Err(ApiError::not_found(format!("no snapshot named {name:?}")))
    }
}

fn io_error(context: String) -> impl FnOnce(io::Error) -> ApiError {
    move |e| ApiError::internal(format!("{context}: {e}"))
}

/// Whether `dir` holds a whole collection, which always has a manifest from the moment it is
/// created, since collections are created aside and renamed into place. Only a manifest that
/// is certainly missing counts as no, any other error is passed on, so nothing is removed on
/// an unclear answer.
fn is_complete(dir: &Path) -> io::Result<bool> {
    match fs::metadata(dir.join(nocturnum_storage::manifest::FILE_NAME)) {
        Ok(m) => Ok(m.is_file()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn remove_if_present(path: &Path) -> io::Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(true)
}

/// Whether `path` exists, with any error other than "not found" passed on, so recovery
/// never acts on a guess.
fn exists(path: &Path) -> Result<bool, ApiError> {
    path.try_exists()
        .map_err(io_error(format!("checking {}", path.display())))
}

/// Whether `path` is a file, treating only "not found" as no.
fn is_file(path: &Path) -> Result<bool, ApiError> {
    match fs::metadata(path) {
        Ok(m) => Ok(m.is_file()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error(format!("checking {}", path.display()))(e)),
    }
}

fn complete(path: &Path) -> Result<bool, ApiError> {
    is_complete(path).map_err(io_error(format!("checking {}", path.display())))
}

/// Waits until `c` is the last handle on its collection, so requests in flight have finished,
/// then closes it, which stops its optimizer and releases its directory.
fn close(mut c: Arc<Collection>) {
    loop {
        match Arc::try_unwrap(c) {
            Ok(c) => {
                drop(c);
                return;
            }
            Err(back) => {
                c = back;
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

impl Registry {
    /// Opens every collection under `data`, creating the layout on first start.
    pub fn open(data: &Path, options: RegistryOptions) -> Result<Registry, ApiError> {
        let collections_dir = data.join("collections");
        let snapshots_dir = options
            .snapshots_dir
            .clone()
            .unwrap_or_else(|| data.join("snapshots"));
        // Compared with `..` and symlinks resolved, before either is created, since deleting a
        // collection removes everything under it.
        let resolved =
            |dir: &Path| resolve(dir).map_err(io_error(format!("resolving {}", dir.display())));
        let (c, s) = (resolved(&collections_dir)?, resolved(&snapshots_dir)?);
        if s.starts_with(&c) || c.starts_with(&s) {
            return Err(ApiError::invalid(format!(
                "the snapshots directory {} and the collections {} must not hold one another",
                s.display(),
                c.display()
            )));
        }
        // Every new directory up the path is synced, so a snapshot root made on another disk
        // survives a power cut along with the snapshots in it.
        for dir in [&collections_dir, &snapshots_dir] {
            create_dir_all_durable(dir).map_err(io_error(format!("creating {}", dir.display())))?;
        }
        // Checked again now both exist, in case anything moved in between.
        let (c, s) = (resolved(&collections_dir)?, resolved(&snapshots_dir)?);
        if s.starts_with(&c) || c.starts_with(&s) {
            return Err(ApiError::invalid(format!(
                "the snapshots directory {} and the collections {} must not hold one another",
                s.display(),
                c.display()
            )));
        }
        let locks = [lock_dir(data)?, lock_dir(&snapshots_dir)?];
        let registry = Registry {
            collections_dir,
            snapshots_dir,
            options,
            open: RwLock::new(BTreeMap::new()),
            changes: Mutex::new(()),
            last_snapshot_ms: Mutex::new(0),
            lost: Mutex::new(BTreeMap::new()),
            schedules: Schedules::load(data)?,
            retention: Mutex::new(()),
            _locks: locks,
        };
        registry.recover()?;
        let mut names = Vec::new();
        let dir = &registry.collections_dir;
        for entry in fs::read_dir(dir).map_err(io_error(format!("listing {}", dir.display())))? {
            let entry = entry.map_err(io_error(format!("listing {}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry
                .file_type()
                .map_err(io_error(format!("checking {}", entry.path().display())))?
                .is_dir();
            if check_name(&name).is_ok() && is_dir {
                names.push(name);
            } else {
                tracing::warn!("skipping {}, not a collection", entry.path().display());
            }
        }
        for name in names {
            let c = registry.open_collection(&name)?;
            write(&registry.open).insert(name, Arc::new(c));
        }
        registry.schedules.adopt(
            |name| read(&registry.open).contains_key(name),
            |name| {
                let newest = registry.snapshots(name).ok()?.pop()?;
                let at = snapshot_time(&newest.name)?;
                Some((newest.name, at))
            },
        )?;
        Ok(registry)
    }

    /// Settles what a crash left part way, see the module docs.
    fn recover(&self) -> Result<(), ApiError> {
        let dir = &self.collections_dir;
        let mut leftovers = Vec::new();
        for entry in fs::read_dir(dir).map_err(io_error(format!("listing {}", dir.display())))? {
            let entry = entry.map_err(io_error(format!("listing {}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                leftovers.push(name);
            }
        }
        leftovers.sort();
        for name in &leftovers {
            if let Some(target) = name.strip_prefix(".replaced-")
                && check_name(target).is_ok()
            {
                self.settle_restore(target)?;
            }
        }
        let mut removed = false;
        for name in &leftovers {
            let path = dir.join(name);
            removed |= remove_if_present(&path)
                .map_err(io_error(format!("removing {}", path.display())))?;
        }
        if removed {
            sync_dir(dir).map_err(io_error(format!("syncing {}", dir.display())))?;
        }
        Ok(())
    }

    /// Settles a restore of `name` that stopped part way: finishes one that had moved the old
    /// copy aside, since the new copy was complete by then, or puts the old copy back if the
    /// new one is missing, and only once a live copy is durably in place removes the rest.
    fn settle_restore(&self, name: &str) -> Result<(), ApiError> {
        let dir = &self.collections_dir;
        let live = dir.join(name);
        let restoring = dir.join(format!(".restoring-{name}"));
        let replaced = dir.join(format!(".replaced-{name}"));
        let sync = || sync_dir(dir).map_err(io_error(format!("syncing {}", dir.display())));
        let had_replaced = exists(&replaced)?;
        let had_restoring = exists(&restoring)?;
        if !(had_replaced || had_restoring) {
            return Ok(());
        }
        if had_replaced && !complete(&live)? {
            // Something may have created the name without a manifest, which is no copy.
            remove_if_present(&live).map_err(io_error(format!("removing {}", live.display())))?;
            let from = if had_restoring {
                tracing::info!("finishing the restore of {name}");
                &restoring
            } else {
                tracing::warn!("putting {name} back after an unfinished restore");
                &replaced
            };
            fs::rename(from, &live).map_err(io_error(format!("restoring {name}")))?;
        }
        if had_replaced && !complete(&live)? {
            return Err(ApiError::internal(format!(
                "{name} has no complete live copy after settling its restore, nothing was removed"
            )));
        }
        // The live name may have been renamed into place just now or just before a crash, so
        // it is made durable before any other copy is removed.
        sync()?;
        for leftover in [&restoring, &replaced] {
            remove_if_present(leftover)
                .map_err(io_error(format!("removing {}", leftover.display())))?;
        }
        sync()
    }

    fn open_collection(&self, name: &str) -> Result<Collection, ApiError> {
        let dir = self.collections_dir.join(name);
        let (mut c, recovery) = Collection::open(&dir, self.options.shard, None)
            .map_err(|e| ApiError::internal(format!("opening collection {name}: {e}")))?;
        tracing::info!(
            "opened collection {name}: {} points, replayed {} operations",
            c.len(),
            recovery.replayed
        );
        if let Some(o) = self.options.optimizer {
            c.start_optimizer(o);
        }
        Ok(c)
    }

    /// Puts a collection still on disk back in service, or records why it could not be.
    ///
    /// A collection with no complete copy at its name is never opened, since opening creates
    /// the directory, which would then hide the recoverable copies from the next start.
    fn reinstate(&self, open: &mut BTreeMap<String, Arc<Collection>>, name: &str) {
        let opened = match is_complete(&self.collections_dir.join(name)) {
            Ok(true) => self.open_collection(name),
            Ok(false) => Err(ApiError::internal(format!(
                "{name} has no complete copy in place"
            ))),
            Err(e) => Err(ApiError::internal(format!("checking {name}: {e}"))),
        };
        match opened {
            Ok(c) => {
                open.insert(name.to_owned(), Arc::new(c));
                lock(&self.lost).remove(name);
            }
            Err(e) => {
                tracing::error!("collection {name} is out of service: {}", e.message);
                lock(&self.lost).insert(
                    name.to_owned(),
                    "it could not be reopened after a failed change, restart the server".into(),
                );
            }
        }
    }

    /// Brings the disk and the open collections back in line before a change to `name`:
    /// settles a restore left part way, then opens a complete copy found on disk that is not
    /// open, so a change never acts on a name whose state is unknown.
    fn reconcile(&self, name: &str) -> Result<(), ApiError> {
        self.settle_restore(name)?;
        if self.schedules.deleting(name) && !read(&self.open).contains_key(name) {
            // The delete may have failed to sync its rename, which has to be durable before
            // the schedule goes, or a power cut could bring the collection back without it.
            let dir = &self.collections_dir;
            sync_dir(dir)
                .map_err(|e| ApiError::unavailable(format!("syncing {}: {e}", dir.display())))?;
            let _retention = self.retention();
            self.schedules.remove(name).map_err(|e| {
                ApiError::unavailable(format!(
                    "the snapshot schedule of the deleted collection {name} could not be \
                     removed: {}",
                    e.message
                ))
            })?;
        }
        if !read(&self.open).contains_key(name) && complete(&self.collections_dir.join(name))? {
            self.reinstate(&mut write(&self.open), name);
        }
        if let Some(why) = lock(&self.lost).get(name) {
            return Err(ApiError::unavailable(format!("collection {name}: {why}")));
        }
        Ok(())
    }

    /// Collections out of service until a restart, with the reason.
    pub fn lost(&self) -> Vec<(String, String)> {
        lock(&self.lost)
            .iter()
            .map(|(n, w)| (n.clone(), w.clone()))
            .collect()
    }

    pub fn get(&self, name: &str) -> Result<Arc<Collection>, ApiError> {
        read(&self.open)
            .get(name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("no collection named {name:?}")))
    }

    /// Every open collection, by name.
    pub fn list(&self) -> Vec<(String, Arc<Collection>)> {
        read(&self.open)
            .iter()
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect()
    }

    /// Creates a collection and returns whether it is new, refusing one that exists with a
    /// different config.
    pub fn create(&self, name: &str, config: VectorConfig) -> Result<bool, ApiError> {
        check_name(name)?;
        config.validate()?;
        let _changes = lock(&self.changes);
        self.reconcile(name)?;
        if let Ok(c) = self.get(name) {
            return if c.config() == config {
                Ok(false)
            } else {
                Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "conflict",
                    format!("collection {name:?} already exists with a different config"),
                ))
            };
        }
        let dir = &self.collections_dir;
        let staging = dir.join(format!(".creating-{}", uuid::Uuid::new_v4()));
        let created = Collection::open(&staging, self.options.shard, Some(config));
        if let Err(e) = created {
            let _ = fs::remove_dir_all(&staging);
            return Err(ApiError::internal(format!(
                "creating collection {name}: {e}"
            )));
        }
        drop(created);
        let live = dir.join(name);
        if let Err(e) = fs::rename(&staging, &live) {
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error(format!("creating collection {name}"))(e));
        }
        // Published from here on, so a failure leaves it in service or recorded as lost.
        let synced = sync_dir(dir);
        self.reinstate(&mut write(&self.open), name);
        synced.map_err(io_error(format!("creating collection {name}")))?;
        if let Some(why) = lock(&self.lost).get(name) {
            return Err(ApiError::unavailable(format!("collection {name}: {why}")));
        }
        Ok(true)
    }

    /// Deletes a collection, once requests using it have finished, and returns whether it
    /// existed. Its snapshots are kept.
    pub fn delete(&self, name: &str) -> Result<bool, ApiError> {
        check_name(name)?;
        let _changes = lock(&self.changes);
        // Settles a restore's leftovers first, or an old copy could come back at the next start.
        self.reconcile(name)?;
        if !read(&self.open).contains_key(name) {
            return Ok(false);
        }
        // The schedule is marked first, durably, so it stops running, and startup keeps it only
        // if the collection is still there, so it never outlives the collection to prune the
        // snapshots of one restored or created later under the same name.
        let _retention = self.retention();
        let scheduled = self.schedules.mark_deleting(name)?;
        let Some(c) = write(&self.open).remove(name) else {
            return Ok(false);
        };
        close(c);
        let dir = &self.collections_dir;
        let trash = dir.join(format!(".deleted-{}", uuid::Uuid::new_v4()));
        let live = dir.join(name);
        if let Err(e) = fs::rename(&live, &trash) {
            // Still in place, so it goes back into service with its schedule.
            self.reinstate(&mut write(&self.open), name);
            if scheduled && let Err(e) = self.schedules.unmark_deleting(name) {
                tracing::warn!("keeping the snapshot schedule of {name}: {}", e.message);
            }
            return Err(io_error(format!("deleting collection {name}"))(e));
        }
        sync_dir(dir).map_err(io_error(format!("deleting collection {name}")))?;
        // Removed now the collection is gone, and if that fails, before the name is used again.
        if scheduled && let Err(e) = self.schedules.remove(name) {
            tracing::warn!("removing the snapshot schedule of {name}: {}", e.message);
        }
        if let Err(e) = fs::remove_dir_all(&trash) {
            // It is out of the way already, and the next start removes it.
            tracing::warn!("removing {}: {e}", trash.display());
        }
        Ok(true)
    }

    fn snapshot_dir(&self, name: &str) -> PathBuf {
        self.snapshots_dir.join(name)
    }

    /// Checks that `name` is a collection or has snapshots, for the snapshot endpoints, which
    /// keep working after a collection is deleted.
    fn snapshot_owner(&self, name: &str) -> Result<PathBuf, ApiError> {
        check_name(name)?;
        let dir = self.snapshot_dir(name);
        if self.get(name).is_ok() || exists(&dir)? {
            Ok(dir)
        } else {
            Err(ApiError::not_found(format!("no collection named {name:?}")))
        }
    }

    /// The path of an existing snapshot, with its sidecar.
    pub fn snapshot_path(&self, name: &str, snapshot: &str) -> Result<PathBuf, ApiError> {
        let dir = self.snapshot_owner(name)?;
        check_snapshot_name(snapshot)?;
        let path = dir.join(snapshot);
        let sidecar = dir.join(format!("{snapshot}.sha256"));
        if is_file(&path)? && is_file(&sidecar)? {
            Ok(path)
        } else {
            Err(ApiError::not_found(format!(
                "no snapshot named {snapshot:?}"
            )))
        }
    }

    pub fn take_snapshot(&self, name: &str) -> Result<SnapshotEntry, ApiError> {
        let c = self.get(name)?;
        self.snapshot_collection(name, &c)
    }

    /// The collection `name`, if `wanted` still says so once no create, delete or restore is
    /// under way, for work that has to stay on the collection it started with.
    pub(crate) fn pin(&self, name: &str, wanted: impl FnOnce() -> bool) -> Option<Arc<Collection>> {
        let _changes = lock(&self.changes);
        if wanted() { self.get(name).ok() } else { None }
    }

    pub(crate) fn snapshot_collection(
        &self,
        name: &str,
        c: &Collection,
    ) -> Result<SnapshotEntry, ApiError> {
        let dir = self.snapshot_dir(name);
        create_dir_all_durable(&dir).map_err(io_error(format!("creating {}", dir.display())))?;
        let file = {
            let mut last = lock(&self.last_snapshot_ms);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            *last = now.max(*last + 1);
            format!("{name}-{:015}.snapshot", *last)
        };
        let info = c
            .snapshot(&dir.join(&file))
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(SnapshotEntry {
            created_at: created_at(&file, &info.path),
            name: file,
            size: info.bytes,
            sha256: info.sha256,
        })
    }

    /// The snapshots of a collection, oldest first.
    pub fn snapshots(&self, name: &str) -> Result<Vec<SnapshotEntry>, ApiError> {
        let dir = self.snapshot_owner(name)?;
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error(format!("listing {}", dir.display()))(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error(format!("listing {}", dir.display())))?;
            let file = entry.file_name().to_string_lossy().into_owned();
            if check_snapshot_name(&file).is_err() {
                continue;
            }
            let path = entry.path();
            let gone = |e: &io::Error| e.kind() == io::ErrorKind::NotFound;
            // A snapshot without a sidecar was never finished, or is being deleted, and one
            // with a damaged sidecar cannot be restored, so neither is listed, while any other
            // error fails the listing rather than hide a backup.
            let sha256 = match snapshot_checksum(&path) {
                Ok(s) => s,
                Err(nocturnum_storage::Error::Io { source, .. }) if gone(&source) => continue,
                Err(nocturnum_storage::Error::Corrupt { .. }) => continue,
                Err(e) => return Err(ApiError::internal(format!("reading {file}: {e}"))),
            };
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) if gone(&e) => continue,
                Err(e) => return Err(io_error(format!("reading {file}"))(e)),
            };
            out.push(SnapshotEntry {
                created_at: created_at(&file, &path),
                name: file,
                size: meta.len(),
                sha256,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn delete_snapshot(&self, name: &str, snapshot: &str) -> Result<(), ApiError> {
        let path = self.snapshot_path(name, snapshot)?;
        remove_snapshot(&path).map_err(|e| ApiError::internal(format!("deleting {snapshot}: {e}")))
    }

    /// Replaces the collection's contents with a snapshot, creating the collection if it was
    /// deleted, see the module docs for how a crash part way is settled.
    ///
    /// Requests for the collection wait while the copies are swapped.
    pub fn restore(&self, name: &str, snapshot: &str) -> Result<(), ApiError> {
        let path = self.snapshot_path(name, snapshot)?;
        let _changes = lock(&self.changes);
        let dir = &self.collections_dir;
        let restoring = dir.join(format!(".restoring-{name}"));
        let replaced = dir.join(format!(".replaced-{name}"));
        // An earlier restore that failed part way may have left the only old copy aside, and
        // a collection that could not be reopened is moved aside like any other.
        if let Err(e) = self.reconcile(name)
            && !lock(&self.lost).contains_key(name)
        {
            return Err(e);
        }
        restore(&path, &restoring).map_err(|e| match e {
            nocturnum_storage::Error::Corrupt { reason, .. } => ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_snapshot",
                format!("snapshot {snapshot:?} cannot be restored: {reason}"),
            ),
            e => ApiError::internal(format!("restoring {snapshot}: {e}")),
        })?;

        let live = dir.join(name);
        // Checked before the live collection is taken out of service, so this error leaves it
        // there.
        let had = exists(&live)?;
        let mut open = write(&self.open);
        if let Some(c) = open.remove(name) {
            close(c);
        }
        let swap = || -> io::Result<()> {
            if had {
                fs::rename(&live, &replaced)?;
                sync_dir(dir)?;
            }
            fs::rename(&restoring, &live)?;
            sync_dir(dir)
        };
        if let Err(e) = swap() {
            // Put the old copy back in service, as startup would.
            if !live.exists() && replaced.exists() {
                let _ = fs::rename(&replaced, &live).and_then(|()| sync_dir(dir));
            }
            // Records the collection as out of service when no copy could be put back.
            self.reinstate(&mut open, name);
            return Err(ApiError::internal(format!("restoring {name}: {e}")));
        }
        self.reinstate(&mut open, name);
        let reopened = open.contains_key(name);
        drop(open);
        if !reopened {
            return Err(ApiError::internal(format!(
                "{name} was restored but could not be opened, restart the server"
            )));
        }
        if let Err(e) = remove_if_present(&replaced) {
            // The restored copy is live and durable, and the next restore or start removes it.
            tracing::warn!("removing {}: {e}", replaced.display());
        }
        Ok(())
    }
}

impl Registry {
    pub(crate) fn schedules(&self) -> &Schedules {
        &self.schedules
    }

    pub(crate) fn retention(&self) -> MutexGuard<'_, ()> {
        lock(&self.retention)
    }

    /// The snapshot schedule of a collection and how its runs went.
    pub fn schedule(&self, name: &str) -> Result<ScheduleState, ApiError> {
        self.get(name)?;
        self.schedules.get(name).ok_or_else(|| {
            ApiError::not_found(format!("collection {name:?} has no snapshot schedule"))
        })
    }

    /// Sets or replaces the snapshot schedule of a collection, whose next run is the first
    /// time the cron names from now on.
    pub fn set_schedule(&self, name: &str, schedule: Schedule) -> Result<ScheduleState, ApiError> {
        let cron = schedule.check()?;
        // Taking turns with delete, so a schedule never outlives its collection.
        let _changes = lock(&self.changes);
        self.get(name)?;
        let _retention = self.retention();
        self.schedules.set(name, schedule, cron, SystemTime::now())
    }

    /// Removes the snapshot schedule of a collection, keeping its snapshots, and returns
    /// whether it had one.
    pub fn delete_schedule(&self, name: &str) -> Result<bool, ApiError> {
        let _changes = lock(&self.changes);
        self.get(name)?;
        let _retention = self.retention();
        self.schedules.remove(name)
    }

    /// Keeps the newest `keep` snapshots of a collection that pass their checksum, and
    /// returns the names of newer ones that failed it, which are left in place.
    pub fn prune_snapshots(&self, name: &str, keep: usize) -> Result<Vec<String>, String> {
        let pruned = prune(&self.snapshot_dir(name), keep).map_err(|e| e.to_string())?;
        Ok(pruned
            .invalid
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect())
    }
}

/// When a snapshot was taken, from the time in its name, or its file's modification time for
/// a name without one.
fn created_at(file: &str, path: &Path) -> String {
    let time = snapshot_time(file)
        .or_else(|| fs::metadata(path).and_then(|m| m.modified()).ok())
        .unwrap_or(UNIX_EPOCH);
    rfc3339(time)
}

/// The time in a snapshot's name, `<collection>-<15 digits of milliseconds>.snapshot`.
fn snapshot_time(file: &str) -> Option<SystemTime> {
    file.strip_suffix(".snapshot")
        .and_then(|s| s.rsplit_once('-'))
        .filter(|(_, ms)| ms.len() == 15)
        .and_then(|(_, ms)| ms.parse::<u64>().ok())
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
}

/// `path` made absolute, walked one part at a time the way the system would, resolving each
/// part that exists, symlinks included, and `.` and `..` in parts that do not.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in std::env::current_dir()?.join(path).components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
        match fs::canonicalize(&out) {
            Ok(real) => out = real,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Takes the exclusive lock in `dir`, held until the file is dropped.
fn lock_dir(dir: &Path) -> Result<fs::File, ApiError> {
    let path = dir.join("LOCK");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error(format!("opening {}", path.display())))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => Err(ApiError::unavailable(format!(
            "{} is in use by another nocturnum server",
            dir.display()
        ))),
        Err(fs::TryLockError::Error(e)) => Err(io_error(format!("locking {}", path.display()))(e)),
    }
}

/// Formats a time as RFC 3339 in UTC to the second, such as `2026-10-11T03:00:00Z`.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (year, month, day) = civil((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point_count(r: &Registry, name: &str) -> usize {
        r.get(name).map(|c| c.len()).unwrap_or(usize::MAX)
    }

    fn with_one_point(dir: &Path) -> Registry {
        let r = Registry::open(dir, RegistryOptions::default()).unwrap();
        let config = VectorConfig {
            dim: 2,
            distance: nocturnum_core::Distance::Dot,
        };
        r.create("a", config).unwrap();
        r.get("a")
            .unwrap()
            .upsert(
                vec![nocturnum_storage::PointWrite {
                    id: 1.into(),
                    vector: vec![1.0, 0.0],
                    payload: Default::default(),
                }],
                nocturnum_core::Ack::Durable,
            )
            .unwrap();
        r
    }

    #[test]
    fn a_restore_after_a_failed_swap_keeps_the_old_copy() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let snap = r.take_snapshot("a").unwrap();
        let snap_path = dir.path().join("snapshots/a").join(&snap.name);
        let mut bytes = fs::read(&snap_path).unwrap();
        bytes[700] ^= 1;
        fs::write(&snap_path, bytes).unwrap();
        // A swap that moved the old copy aside and then failed, with the rollback failing
        // too.
        close(write(&r.open).remove("a").unwrap());
        let cols = dir.path().join("collections");
        fs::rename(cols.join("a"), cols.join(".replaced-a")).unwrap();

        let e = r.restore("a", &snap.name).unwrap_err();
        assert_eq!(e.code, "invalid_snapshot");
        assert_eq!(point_count(&r, "a"), 1, "the old copy is back in service");
        assert!(!cols.join(".replaced-a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_delete_keeps_the_collection_in_service() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let cols = dir.path().join("collections");
        fs::set_permissions(&cols, fs::Permissions::from_mode(0o555)).unwrap();
        let deleted = r.delete("a");
        fs::set_permissions(&cols, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(deleted.is_err());
        assert_eq!(point_count(&r, "a"), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_collection_that_cannot_be_reopened_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let cols = dir.path().join("collections");
        let mode = |p: &Path, m| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        // The rename fails on a read-only parent, and reopening fails on an unreadable copy.
        mode(&cols.join("a"), 0o000);
        mode(&cols, 0o555);
        let deleted = r.delete("a");
        mode(&cols, 0o755);
        mode(&cols.join("a"), 0o755);
        assert!(deleted.is_err());
        assert!(r.get("a").is_err());
        assert_eq!(r.lost().len(), 1);
        assert!(cols.join("a/MANIFEST").is_file(), "nothing was removed");
        // Once the copy can be read again, the next change to the name puts it back first.
        let config = VectorConfig {
            dim: 2,
            distance: nocturnum_core::Distance::Dot,
        };
        assert!(!r.create("a", config).unwrap());
        assert_eq!(point_count(&r, "a"), 1);
        assert!(r.lost().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_name_whose_state_is_unclear_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let cols = dir.path().join("collections");
        fs::set_permissions(&cols, fs::Permissions::from_mode(0o444)).unwrap();
        let deleted = r.delete("a");
        fs::set_permissions(&cols, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(deleted.is_err());
        assert_eq!(point_count(&r, "a"), 1, "still in service");
        assert!(r.lost().is_empty());
    }

    #[test]
    fn a_delete_settles_restore_leftovers_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let cols = dir.path().join("collections");
        // A restore that succeeded but could not remove the old copy.
        fs::create_dir_all(cols.join(".replaced-a/old")).unwrap();
        assert!(r.delete("a").unwrap());
        drop(r);
        let r = Registry::open(dir.path(), RegistryOptions::default()).unwrap();
        assert!(r.get("a").is_err(), "the old copy did not come back");
    }

    #[test]
    fn a_schedule_left_by_a_delete_is_removed_before_the_name_is_reused() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let schedule = crate::Schedule {
            cron: "@daily".into(),
            keep: 1,
        };
        r.set_schedule("a", schedule).unwrap();
        // A delete that moved the collection away but could not remove its schedule.
        assert!(r.schedules.mark_deleting("a").unwrap());
        close(write(&r.open).remove("a").unwrap());
        let cols = dir.path().join("collections");
        fs::rename(cols.join("a"), cols.join(".deleted-x")).unwrap();
        let config = VectorConfig {
            dim: 2,
            distance: nocturnum_core::Distance::Dot,
        };
        assert!(r.create("a", config).unwrap());
        assert_eq!(r.schedule("a").unwrap_err().code, "not_found");
        drop(r);
        let r = Registry::open(dir.path(), RegistryOptions::default()).unwrap();
        assert_eq!(r.schedule("a").unwrap_err().code, "not_found");
    }

    #[test]
    fn create_never_claims_a_name_holding_recovery_copies() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let cols = dir.path().join("collections");
        // A restore whose swap and rollback both failed: the old copy is aside and the name
        // is empty.
        close(write(&r.open).remove("a").unwrap());
        fs::rename(cols.join("a"), cols.join(".replaced-a")).unwrap();
        let config = VectorConfig {
            dim: 2,
            distance: nocturnum_core::Distance::Dot,
        };
        assert!(!r.create("a", config).unwrap(), "the old copy was put back");
        assert_eq!(point_count(&r, "a"), 1);
    }

    #[test]
    fn a_collection_with_no_copy_left_is_reported() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        r.reinstate(&mut write(&r.open), "gone");
        assert_eq!(r.lost().len(), 1);
        assert!(
            !dir.path().join("collections/gone").exists(),
            "nothing was created at the name"
        );
        close(write(&r.open).remove("a").unwrap());
        r.reinstate(&mut write(&r.open), "a");
        assert_eq!(r.lost().len(), 1, "a reopened collection is not lost");
        assert_eq!(point_count(&r, "a"), 1);
    }

    #[test]
    fn a_name_without_a_manifest_does_not_hide_the_old_copy() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        drop(r);
        let cols = dir.path().join("collections");
        fs::rename(cols.join("a"), cols.join(".replaced-a")).unwrap();
        fs::create_dir_all(cols.join("a/wal")).unwrap();
        let r = Registry::open(dir.path(), RegistryOptions::default()).unwrap();
        assert_eq!(point_count(&r, "a"), 1);
    }

    #[cfg(unix)]
    #[test]
    fn an_unclear_live_copy_stops_recovery_without_removing_anything() {
        let dir = tempfile::TempDir::new().unwrap();
        drop(with_one_point(dir.path()));
        let cols = dir.path().join("collections");
        fs::create_dir_all(cols.join(".replaced-a/old")).unwrap();
        // A manifest whose lookup fails with something other than "not found".
        let manifest = cols.join("a/MANIFEST");
        fs::rename(&manifest, cols.join("a/MANIFEST.real")).unwrap();
        std::os::unix::fs::symlink("MANIFEST", &manifest).unwrap();
        assert!(Registry::open(dir.path(), RegistryOptions::default()).is_err());
        assert!(cols.join("a/MANIFEST.real").is_file());
        assert!(cols.join(".replaced-a/old").is_dir());
    }

    #[test]
    fn a_live_copy_wins_over_leftovers() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        drop(r);
        let cols = dir.path().join("collections");
        fs::create_dir_all(cols.join(".replaced-a/old")).unwrap();
        fs::create_dir_all(cols.join(".restoring-a/new")).unwrap();
        let r = Registry::open(dir.path(), RegistryOptions::default()).unwrap();
        assert_eq!(point_count(&r, "a"), 1);
        assert!(!cols.join(".replaced-a").exists() && !cols.join(".restoring-a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_listing_fails_rather_than_hide_a_backup() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let snap = r.take_snapshot("a").unwrap();
        let sidecar = dir
            .path()
            .join("snapshots/a")
            .join(format!("{}.sha256", snap.name));
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o000)).unwrap();
        let listed = r.snapshots("a");
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(listed.is_err());
        // A lookup that fails for another reason than "not found" is an error, not a 404.
        let snaps = dir.path().join("snapshots/a");
        fs::set_permissions(&snaps, fs::Permissions::from_mode(0o000)).unwrap();
        let found = r.snapshot_path("a", &snap.name);
        fs::set_permissions(&snaps, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(found.unwrap_err().code, "internal");
        // A damaged sidecar marks a snapshot that cannot be restored, which is left out.
        fs::write(&sidecar, "not a checksum\n").unwrap();
        assert_eq!(r.snapshots("a").unwrap(), []);
        fs::remove_file(&sidecar).unwrap();
        assert_eq!(r.snapshots("a").unwrap(), []);
    }

    #[test]
    fn snapshots_in_the_same_millisecond_get_their_own_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_one_point(dir.path());
        let names: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4).map(|_| s.spawn(|| r.take_snapshot("a"))).collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().unwrap().name)
                .collect()
        });
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 4, "{names:?}");
        assert_eq!(r.snapshots("a").unwrap().len(), 4);
    }

    #[test]
    fn names() {
        for ok in ["a", "notes", "my_notes-2", "0day", &"a".repeat(63)] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Notes",
            "-a",
            "_a",
            ".a",
            "a/b",
            "a.b",
            "a b",
            &"a".repeat(64),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn snapshot_names() {
        assert!(check_snapshot_name("notes-001760000000000.snapshot").is_ok());
        for bad in [
            "../x.snapshot",
            ".snapshot",
            "a/b.snapshot",
            "notes.tar",
            "x.snapshot.sha256",
        ] {
            assert!(check_snapshot_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dates() {
        let at = |s| rfc3339(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_791_687_600), "2026-10-11T03:00:00Z");
        assert_eq!(at(4_107_542_399), "2100-02-28T23:59:59Z");
        assert_eq!(
            created_at("notes-001791687600000.snapshot", Path::new("/nonexistent")),
            "2026-10-11T03:00:00Z"
        );
    }
}
