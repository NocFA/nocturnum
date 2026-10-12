//! Scheduled snapshots: a cron schedule and a number of snapshots to keep per collection,
//! stored in `schedules.json` in the data directory, and the thread that runs them.
//!
//! A schedule's next run is the first time its cron names after a base time: when the
//! schedule was set, raised at startup to the newest snapshot on disk, and moved to the end of
//! each run. A run missed while the server was down therefore happens once, straight after
//! the next start, and a run that outlasts the next scheduled time skips it.
//!
//! Runs take turns on one thread, so one collection never has two scheduled snapshots at once.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cron::Cron;
use crate::error::ApiError;
use crate::registry::{Registry, rfc3339};

/// The most snapshots a schedule keeps.
pub const MAX_KEEP: usize = 1000;
const FILE_NAME: &str = "schedules.json";
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// The longest the thread sleeps before looking at the clock again, so a clock change or a
/// suspended machine never delays a run by long.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// A schedule as the API takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub cron: String,
    pub keep: usize,
}

impl Schedule {
    pub(crate) fn check(&self) -> Result<Cron, ApiError> {
        if !(1..=MAX_KEEP).contains(&self.keep) {
            return Err(ApiError::invalid(format!(
                "keep must be between 1 and {MAX_KEEP}, got {}",
                self.keep
            ))
            .at("keep"));
        }
        Cron::parse(&self.cron).map_err(|e| ApiError::invalid(e).at("cron"))
    }
}

/// A schedule with how its runs went, as the API reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduleState {
    pub cron: String,
    pub keep: usize,
    pub next_run: Option<String>,
    pub running: bool,
    pub last_run: Option<String>,
    pub last_success: Option<String>,
    pub last_snapshot: Option<String>,
    pub last_error: Option<String>,
    /// Kept snapshots that failed their checksum at the last run, left in place.
    pub damaged: Vec<String>,
}

/// One schedule's figures for `/metrics`.
pub(crate) struct ScheduleGauges {
    pub running: bool,
    pub next_run: Option<SystemTime>,
    pub last_success: Option<SystemTime>,
    pub failures: u64,
    pub damaged: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    cron: String,
    keep: usize,
    /// When the schedule was set, in milliseconds since 1970.
    since_ms: u64,
    /// Set while its collection is being deleted, see `Registry::delete`.
    #[serde(default, skip_serializing_if = "is_false")]
    deleting: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFile {
    version: u32,
    schedules: BTreeMap<String, Stored>,
}

#[derive(Debug, Clone, Default)]
struct Status {
    running: bool,
    last_run: Option<SystemTime>,
    last_success: Option<SystemTime>,
    last_snapshot: Option<String>,
    last_error: Option<String>,
    damaged: Vec<String>,
    failures: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct Job {
    /// Changes whenever the schedule is set, so a run queued under an older one is skipped.
    revision: u64,
    schedule: Schedule,
    cron: Cron,
    since: SystemTime,
    base: SystemTime,
    status: Status,
    /// Its collection is being deleted, so it never runs, and startup keeps it only if the
    /// collection is still there.
    deleting: bool,
}

impl Job {
    fn next_run(&self) -> Option<SystemTime> {
        self.cron.next_after(self.base)
    }

    fn state(&self) -> ScheduleState {
        let s = &self.status;
        ScheduleState {
            cron: self.schedule.cron.clone(),
            keep: self.schedule.keep,
            next_run: self.next_run().map(rfc3339),
            running: s.running,
            last_run: s.last_run.map(rfc3339),
            last_success: s.last_success.map(rfc3339),
            last_snapshot: s.last_snapshot.clone(),
            last_error: s.last_error.clone(),
            damaged: s.damaged.clone(),
        }
    }
}

/// How a run went.
pub(crate) struct Outcome {
    pub snapshot: Result<String, String>,
    /// Pruning, when the snapshot was taken: the damaged snapshots it found, or an error.
    pub pruned: Option<Result<Vec<String>, String>>,
}

#[derive(Default)]
struct Jobs {
    jobs: BTreeMap<String, Job>,
    stop: bool,
}

pub(crate) struct Schedules {
    path: PathBuf,
    jobs: Mutex<Jobs>,
    wake: Condvar,
    revisions: AtomicU64,
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Schedules {
    /// Reads the schedules kept in `data`, refusing a file that cannot be read in full, since
    /// starting without them would silently stop backups.
    pub(crate) fn load(data: &Path) -> Result<Schedules, ApiError> {
        let path = data.join(FILE_NAME);
        let fail = |why: String| ApiError::internal(format!("reading {}: {why}", path.display()));
        let mut jobs = BTreeMap::new();
        match fs::File::open(&path) {
            Ok(f) => {
                let mut text = String::new();
                f.take(MAX_FILE_BYTES + 1)
                    .read_to_string(&mut text)
                    .map_err(|e| fail(e.to_string()))?;
                if text.len() as u64 > MAX_FILE_BYTES {
                    return Err(fail("the file is too large".into()));
                }
                let stored: StoredFile =
                    serde_json::from_str(&text).map_err(|e| fail(e.to_string()))?;
                if stored.version != 1 {
                    return Err(fail(format!("unknown version {}", stored.version)));
                }
                for (name, s) in stored.schedules {
                    let schedule = Schedule {
                        cron: s.cron,
                        keep: s.keep,
                    };
                    let cron = schedule
                        .check()
                        .map_err(|e| fail(format!("the schedule of {name}: {}", e.message)))?;
                    let since = UNIX_EPOCH + Duration::from_millis(s.since_ms);
                    jobs.insert(
                        name,
                        Job {
                            revision: jobs.len() as u64 + 1,
                            schedule,
                            cron,
                            since,
                            base: since,
                            status: Status::default(),
                            deleting: s.deleting,
                        },
                    );
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(fail(e.to_string())),
        }
        Ok(Schedules {
            path,
            revisions: AtomicU64::new(jobs.len() as u64 + 1),
            jobs: Mutex::new(Jobs { jobs, stop: false }),
            wake: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Jobs> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes the schedules aside, syncs them and renames them into place.
    fn persist(&self, jobs: &BTreeMap<String, Job>) -> io::Result<()> {
        let stored = StoredFile {
            version: 1,
            schedules: jobs
                .iter()
                .map(|(name, j)| {
                    let s = Stored {
                        cron: j.schedule.cron.clone(),
                        keep: j.schedule.keep,
                        since_ms: millis(j.since),
                        deleting: j.deleting,
                    };
                    (name.clone(), s)
                })
                .collect(),
        };
        let mut text = serde_json::to_vec_pretty(&stored).map_err(io::Error::other)?;
        text.push(b'\n');
        let tmp = self.path.with_extension("json.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&text)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &self.path)?;
        nocturnum_storage::sync_dir(self.path.parent().unwrap_or(Path::new(".")))
    }

    /// Changes the jobs with `f` and saves them, putting the old ones back if saving fails.
    fn change<T>(&self, f: impl FnOnce(&mut BTreeMap<String, Job>) -> T) -> Result<T, ApiError> {
        let mut g = self.lock();
        let before = g.jobs.clone();
        let out = f(&mut g.jobs);
        if let Err(e) = self.persist(&g.jobs) {
            g.jobs = before;
            return Err(ApiError::internal(format!(
                "saving {}: {e}",
                self.path.display()
            )));
        }
        self.wake.notify_all();
        Ok(out)
    }

    /// Keeps only the schedules whose collection `exists`, saving the change, so a schedule
    /// whose collection was deleted can never come back to prune a later collection's
    /// snapshots, puts back in service one whose delete stopped before the collection went,
    /// and starts each one's runs from the newest snapshot it already has, given by `newest`.
    pub(crate) fn adopt(
        &self,
        exists: impl Fn(&str) -> bool,
        newest: impl Fn(&str) -> Option<(String, SystemTime)>,
    ) -> Result<(), ApiError> {
        let (orphans, survivors): (Vec<String>, Vec<String>) = {
            let g = self.lock();
            let orphans = g.jobs.keys().filter(|n| !exists(n)).cloned().collect();
            let survivors = g
                .jobs
                .iter()
                .filter(|(n, j)| j.deleting && exists(n))
                .map(|(n, _)| n.clone())
                .collect();
            (orphans, survivors)
        };
        if !orphans.is_empty() {
            tracing::warn!("dropping the snapshot schedules of {orphans:?}, no such collection");
        }
        if !survivors.is_empty() {
            tracing::warn!("keeping the snapshot schedules of {survivors:?}, their delete stopped");
        }
        if !(orphans.is_empty() && survivors.is_empty()) {
            self.change(|jobs| {
                jobs.retain(|name, _| !orphans.contains(name));
                for name in &survivors {
                    if let Some(j) = jobs.get_mut(name) {
                        j.deleting = false;
                    }
                }
            })?;
        }
        let mut g = self.lock();
        for (name, job) in &mut g.jobs {
            if let Some((file, at)) = newest(name) {
                job.base = job.base.max(at);
                job.status.last_success = Some(at);
                job.status.last_snapshot = Some(file);
            }
        }
        Ok(())
    }

    /// How many snapshots the schedule of `name` keeps, `None` once it has none or has been
    /// set again since `revision`.
    fn keep(&self, name: &str, revision: u64) -> Option<usize> {
        self.lock()
            .jobs
            .get(name)
            .filter(|j| j.revision == revision && !j.deleting)
            .map(|j| j.schedule.keep)
    }

    /// Marks the schedule of `name`, if any, as being deleted along with its collection, and
    /// returns whether there was one.
    pub(crate) fn mark_deleting(&self, name: &str) -> Result<bool, ApiError> {
        if !self.lock().jobs.contains_key(name) {
            return Ok(false);
        }
        self.change(|jobs| {
            if let Some(j) = jobs.get_mut(name) {
                j.deleting = true;
            }
        })?;
        Ok(true)
    }

    /// Puts a schedule marked by [`Schedules::mark_deleting`] back in service, after a delete
    /// that failed, as a new revision, so a run queued before is skipped.
    pub(crate) fn unmark_deleting(&self, name: &str) -> Result<(), ApiError> {
        let revision = self.revisions.fetch_add(1, Ordering::Relaxed);
        let mut g = self.lock();
        if let Some(j) = g.jobs.get_mut(name) {
            j.deleting = false;
            j.revision = revision;
            j.status.running = false;
        }
        let saved = self.persist(&g.jobs);
        self.wake.notify_all();
        // Kept in service either way, and startup finds the collection still there.
        saved.map_err(|e| ApiError::internal(format!("saving {}: {e}", self.path.display())))
    }

    /// Whether `name` has a schedule left from deleting its collection.
    pub(crate) fn deleting(&self, name: &str) -> bool {
        self.lock().jobs.get(name).is_some_and(|j| j.deleting)
    }

    pub(crate) fn get(&self, name: &str) -> Option<ScheduleState> {
        self.lock().jobs.get(name).map(Job::state)
    }

    pub(crate) fn set(
        &self,
        name: &str,
        schedule: Schedule,
        cron: Cron,
        now: SystemTime,
    ) -> Result<ScheduleState, ApiError> {
        let revision = self.revisions.fetch_add(1, Ordering::Relaxed);
        self.change(|jobs| {
            // A run still going under the old schedule stays marked, so the new one waits for
            // it to end.
            let status = jobs.get(name).map(|j| j.status.clone()).unwrap_or_default();
            let job = Job {
                revision,
                schedule,
                cron,
                since: now,
                base: now,
                status,
                deleting: false,
            };
            let state = job.state();
            jobs.insert(name.to_owned(), job);
            state
        })
    }

    pub(crate) fn remove(&self, name: &str) -> Result<bool, ApiError> {
        if !self.lock().jobs.contains_key(name) {
            return Ok(false);
        }
        self.change(|jobs| jobs.remove(name).is_some())
    }

    /// Marks every schedule due at `now` as running and returns their names and revisions.
    pub(crate) fn take_due(&self, now: SystemTime) -> Vec<(String, u64)> {
        let mut g = self.lock();
        if g.stop {
            return Vec::new();
        }
        let mut due = Vec::new();
        for (name, job) in &mut g.jobs {
            if !job.status.running && !job.deleting && job.next_run().is_some_and(|t| t <= now) {
                job.status.running = true;
                due.push((name.clone(), job.revision));
            }
        }
        due
    }

    /// Records how a run of `revision` that ended at `at` went, or only that it ended when
    /// the schedule has been set again since.
    pub(crate) fn finish(&self, name: &str, revision: u64, at: SystemTime, outcome: Outcome) {
        let mut g = self.lock();
        let Some(job) = g.jobs.get_mut(name) else {
            return;
        };
        let s = &mut job.status;
        s.running = false;
        self.wake.notify_all();
        if job.revision != revision {
            return;
        }
        s.last_run = Some(at);
        job.base = job.base.max(at);
        let mut error = None;
        match outcome.snapshot {
            Ok(file) => {
                s.last_success = Some(at);
                s.last_snapshot = Some(file);
            }
            Err(e) => error = Some(format!("taking the snapshot: {e}")),
        }
        match outcome.pruned {
            Some(Ok(damaged)) => s.damaged = damaged,
            Some(Err(e)) => error = Some(format!("removing old snapshots: {e}")),
            None => {}
        }
        if error.is_some() {
            s.failures += 1;
        }
        s.last_error = error;
    }

    pub(crate) fn gauges(&self) -> Vec<(String, ScheduleGauges)> {
        self.lock()
            .jobs
            .iter()
            .map(|(name, j)| {
                let s = &j.status;
                let g = ScheduleGauges {
                    running: s.running,
                    next_run: j.next_run(),
                    last_success: s.last_success,
                    failures: s.failures,
                    damaged: s.damaged.len(),
                };
                (name.clone(), g)
            })
            .collect()
    }

    /// Sleeps until the next run is due, a schedule changes or [`Schedules::stop`] is
    /// called, and returns false once stopped.
    fn wait(&self) -> bool {
        let g = self.lock();
        if g.stop {
            return false;
        }
        let now = SystemTime::now();
        let next = g
            .jobs
            .values()
            .filter(|j| !j.status.running && !j.deleting)
            .filter_map(Job::next_run)
            .min();
        let sleep = next
            .map_or(MAX_SLEEP, |t| {
                t.duration_since(now).unwrap_or(Duration::ZERO)
            })
            .min(MAX_SLEEP);
        let (g, _) = self
            .wake
            .wait_timeout(g, sleep)
            .unwrap_or_else(|e| e.into_inner());
        !g.stop
    }

    fn stop(&self) {
        self.lock().stop = true;
        self.wake.notify_all();
    }

    fn stopped(&self) -> bool {
        self.lock().stop
    }
}

/// The thread running scheduled snapshots, stopped and joined on drop, which waits for a
/// snapshot in progress to finish.
pub struct Scheduler {
    registry: Arc<Registry>,
    thread: Option<JoinHandle<()>>,
}

impl Scheduler {
    pub fn start(registry: Arc<Registry>) -> Scheduler {
        let r = registry.clone();
        let thread = std::thread::Builder::new()
            .name("nocturnum-snapshots".into())
            .spawn(move || {
                loop {
                    r.run_due_schedules(SystemTime::now());
                    if !r.schedules().wait() {
                        break;
                    }
                }
            })
            .expect("starting the snapshot scheduler thread");
        Scheduler {
            registry,
            thread: Some(thread),
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.registry.schedules().stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Registry {
    /// Runs every schedule due at `now`, one after another, unless the scheduler is stopping.
    pub fn run_due_schedules(&self, now: SystemTime) {
        let schedules = self.schedules();
        for (name, revision) in schedules.take_due(now) {
            let outcome = if schedules.stopped() {
                Outcome {
                    snapshot: Err("the server is shutting down".into()),
                    pruned: None,
                }
            } else {
                self.run_schedule(&name, revision)
            };
            // Never before `now`, so a run is not due again at the time it was due.
            schedules.finish(&name, revision, SystemTime::now().max(now), outcome);
        }
    }

    /// Takes a snapshot of `name` and prunes its snapshots, checking first that the schedule
    /// is still the one that was due, since runs due together queue up and one may outlive a
    /// change, and holding the retention lock while pruning, which changing a schedule takes
    /// too, so no prune ever runs under a schedule that has gone or changed.
    fn run_schedule(&self, name: &str, revision: u64) -> Outcome {
        let gone = || Outcome {
            snapshot: Err("the schedule was changed or removed".into()),
            pruned: None,
        };
        // Held while it snapshots, so the collection cannot be deleted and another made under
        // its name in between.
        let Some(c) = self.pin(name, || self.schedules().keep(name, revision).is_some()) else {
            return gone();
        };
        tracing::info!("taking the scheduled snapshot of {name}");
        let snapshot = self.snapshot_collection(name, &c);
        drop(c);
        let snapshot = match snapshot {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("the scheduled snapshot of {name} failed: {}", e.message);
                return Outcome {
                    snapshot: Err(e.message),
                    pruned: None,
                };
            }
        };
        let _retention = self.retention();
        let Some(keep) = self.schedules().keep(name, revision) else {
            return gone();
        };
        let pruned = self.prune_snapshots(name, keep).map_err(|e| {
            tracing::error!("removing old snapshots of {name}: {e}");
            "failed, the server log has the details".to_owned()
        });
        if let Ok(damaged) = &pruned
            && !damaged.is_empty()
        {
            tracing::warn!("snapshots of {name} that fail their checksum: {damaged:?}");
        }
        Outcome {
            snapshot: Ok(snapshot.name),
            pruned: Some(pruned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RegistryOptions;
    use nocturnum_core::{Ack, Distance, VectorConfig};
    use nocturnum_storage::PointWrite;

    const DAY: Duration = Duration::from_secs(86_400);

    fn open(dir: &Path) -> Registry {
        Registry::open(dir, RegistryOptions::default()).unwrap()
    }

    fn with_collection(dir: &Path) -> Registry {
        let r = open(dir);
        let config = VectorConfig {
            dim: 2,
            distance: Distance::Dot,
        };
        r.create("a", config).unwrap();
        r.get("a")
            .unwrap()
            .upsert(
                vec![PointWrite {
                    id: 1.into(),
                    vector: vec![1.0, 0.0],
                    sparse: None,
                    payload: Default::default(),
                }],
                Ack::Durable,
            )
            .unwrap();
        r
    }

    /// Creates collection `a` again in an open registry.
    fn with_collection_named(r: &Registry) {
        let config = VectorConfig {
            dim: 2,
            distance: Distance::Dot,
        };
        assert!(r.create("a", config).unwrap());
    }

    fn daily(keep: usize) -> Schedule {
        Schedule {
            cron: "0 3 * * *".into(),
            keep,
        }
    }

    fn next_run(r: &Registry) -> SystemTime {
        r.schedules().gauges()[0].1.next_run.unwrap()
    }

    fn names(r: &Registry) -> Vec<String> {
        r.snapshots("a")
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// Moves when the schedule of `a` was set, as if set `ago` in the past.
    fn set_since(dir: &Path, ago: Duration) {
        let path = dir.join(FILE_NAME);
        let mut file: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let since = millis(SystemTime::now() - ago);
        file["schedules"]["a"]["since_ms"] = since.into();
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    }

    #[test]
    fn a_due_schedule_snapshots_and_keeps_the_newest() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        for _ in 0..3 {
            r.take_snapshot("a").unwrap();
        }
        let state = r.set_schedule("a", daily(2)).unwrap();
        assert!(state.next_run.as_deref().unwrap().ends_with("T03:00:00Z"));
        assert_eq!(state.last_run, None);

        let due = next_run(&r);
        r.run_due_schedules(due - Duration::from_secs(1));
        assert_eq!(names(&r).len(), 3, "nothing ran before it was due");

        r.run_due_schedules(due);
        let snaps = names(&r);
        assert_eq!(snaps.len(), 2, "{snaps:?}");
        let state = r.schedule("a").unwrap();
        assert_eq!(state.last_snapshot.as_ref(), snaps.last());
        assert_eq!(state.last_error, None);
        assert!(!state.running);
        assert!(state.last_run.is_some() && state.last_success.is_some());
        assert_eq!(next_run(&r), due + DAY);

        r.run_due_schedules(due);
        assert_eq!(names(&r), snaps, "a run is not repeated at the same time");
    }

    #[test]
    fn damaged_snapshots_are_reported_and_kept() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        let good = r.take_snapshot("a").unwrap().name;
        let bad = r.take_snapshot("a").unwrap().name;
        let path = dir.path().join("snapshots/a").join(&bad);
        let mut bytes = fs::read(&path).unwrap();
        bytes[700] ^= 1;
        fs::write(&path, bytes).unwrap();
        r.set_schedule("a", daily(2)).unwrap();
        r.run_due_schedules(next_run(&r));

        let state = r.schedule("a").unwrap();
        assert_eq!(state.damaged, std::slice::from_ref(&bad));
        let left = names(&r);
        assert_eq!(left.len(), 3, "the damaged one is not counted: {left:?}");
        assert!(left.contains(&good) && left.contains(&bad));
        assert_eq!(r.schedules().gauges()[0].1.damaged, 1);
    }

    #[test]
    fn a_run_missed_while_down_happens_once_at_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(5)).unwrap();
        drop(r);
        set_since(dir.path(), 2 * DAY);

        let r = open(dir.path());
        let now = SystemTime::now();
        assert!(next_run(&r) <= now);
        r.run_due_schedules(now);
        r.run_due_schedules(now);
        assert_eq!(names(&r).len(), 1);
        assert!(next_run(&r) > now);
    }

    #[test]
    fn a_newer_snapshot_on_disk_counts_as_the_last_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(5)).unwrap();
        let taken = r.take_snapshot("a").unwrap();
        drop(r);
        set_since(dir.path(), 2 * DAY);

        let r = open(dir.path());
        assert!(next_run(&r) > SystemTime::now());
        let state = r.schedule("a").unwrap();
        assert_eq!(state.last_snapshot, Some(taken.name));
        assert!(state.last_success.is_some());
    }

    #[test]
    fn schedules_are_kept_until_their_collection_goes() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(3)).unwrap();
        drop(r);
        let r = open(dir.path());
        assert_eq!(r.schedule("a").unwrap().keep, 3);
        assert!(r.delete("a").unwrap());
        assert_eq!(r.schedules().gauges().len(), 0);
        drop(r);
        let r = with_collection(dir.path());
        assert_eq!(r.schedule("a").unwrap_err().code, "not_found");
    }

    #[test]
    fn a_delete_stopped_by_a_crash_keeps_the_schedule() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(3)).unwrap();
        assert!(r.schedules().mark_deleting("a").unwrap());
        r.run_due_schedules(next_run(&r));
        assert_eq!(names(&r).len(), 0, "a schedule being deleted never runs");
        drop(r);
        let r = open(dir.path());
        assert!(!r.schedules().deleting("a"));
        r.run_due_schedules(next_run(&r));
        assert_eq!(names(&r).len(), 1);
        drop(r);
        assert!(
            !open(dir.path()).schedules().deleting("a"),
            "the change was saved"
        );
    }

    #[test]
    fn schedules_of_missing_collections_are_dropped_at_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(3)).unwrap();
        drop(r);
        let path = dir.path().join(FILE_NAME);
        let text = fs::read_to_string(&path).unwrap().replace("\"a\"", "\"b\"");
        fs::write(&path, text).unwrap();
        let r = open(dir.path());
        assert!(r.schedules().gauges().is_empty());
    }

    #[test]
    fn bad_schedules_are_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        let e = r
            .set_schedule(
                "a",
                Schedule {
                    cron: "0 3 * *".into(),
                    keep: 1,
                },
            )
            .unwrap_err();
        assert_eq!((e.code, e.at.as_deref()), ("invalid_request", Some("cron")));
        let e = r.set_schedule("a", daily(0)).unwrap_err();
        assert_eq!(e.at.as_deref(), Some("keep"));
        let e = r.set_schedule("a", daily(MAX_KEEP + 1)).unwrap_err();
        assert_eq!(e.at.as_deref(), Some("keep"));
        assert_eq!(r.set_schedule("b", daily(1)).unwrap_err().code, "not_found");
        assert_eq!(r.schedule("a").unwrap_err().code, "not_found");
        assert!(!r.delete_schedule("a").unwrap());
        assert!(!dir.path().join(FILE_NAME).exists());
    }

    #[test]
    fn an_unreadable_schedule_file_stops_startup() {
        let dir = tempfile::TempDir::new().unwrap();
        drop(with_collection(dir.path()));
        fs::write(dir.path().join(FILE_NAME), "{not json").unwrap();
        assert!(Registry::open(dir.path(), RegistryOptions::default()).is_err());
        let bad_cron =
            r#"{"version": 1, "schedules": {"a": {"cron": "x", "keep": 1, "since_ms": 0}}}"#;
        fs::write(dir.path().join(FILE_NAME), bad_cron).unwrap();
        assert!(Registry::open(dir.path(), RegistryOptions::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_run_is_reported_and_waits_for_the_next_time() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(3)).unwrap();
        let snaps = dir.path().join("snapshots");
        fs::set_permissions(&snaps, fs::Permissions::from_mode(0o555)).unwrap();
        let due = next_run(&r);
        r.run_due_schedules(due);
        fs::set_permissions(&snaps, fs::Permissions::from_mode(0o755)).unwrap();

        let state = r.schedule("a").unwrap();
        assert!(state.last_error.unwrap().starts_with("taking the snapshot"));
        assert_eq!(state.last_success, None);
        assert_eq!(r.schedules().gauges()[0].1.failures, 1);
        assert_eq!(next_run(&r), due + DAY);

        // The next run succeeds and clears the error.
        r.run_due_schedules(due + DAY);
        let state = r.schedule("a").unwrap();
        assert_eq!(state.last_error, None);
        assert_eq!(names(&r).len(), 1);
    }

    #[test]
    fn a_queued_run_never_outlives_its_schedule() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        for _ in 0..3 {
            r.take_snapshot("a").unwrap();
        }
        r.set_schedule("a", daily(1)).unwrap();
        let due = next_run(&r);
        // Queued, then the schedule is removed and the collection recreated before it runs.
        let queued = r.schedules().take_due(due);
        assert_eq!(queued.len(), 1);
        assert!(r.delete("a").unwrap());
        with_collection_named(&r);
        let outcome = r.run_schedule("a", queued[0].1);
        assert!(outcome.snapshot.is_err() && outcome.pruned.is_none());
        assert_eq!(names(&r).len(), 3, "nothing was pruned");
    }

    #[test]
    fn a_queued_run_never_runs_a_replaced_schedule() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        for _ in 0..3 {
            r.take_snapshot("a").unwrap();
        }
        r.set_schedule("a", daily(1)).unwrap();
        let due = next_run(&r);
        let queued = r.schedules().take_due(due);
        let yearly = Schedule {
            cron: "@yearly".into(),
            keep: 7,
        };
        r.set_schedule("a", yearly).unwrap();
        r.run_due_schedules(due);
        assert!(
            r.schedule("a").unwrap().running,
            "the new one waits for the old run"
        );
        r.schedules()
            .finish("a", queued[0].1, due, r.run_schedule("a", queued[0].1));
        let state = r.schedule("a").unwrap();
        assert!(!state.running);
        assert_eq!(
            state.last_run, None,
            "the old run's outcome is not recorded"
        );
        assert!(state.next_run.unwrap().ends_with("-01-01T00:00:00Z"));
        assert_eq!(names(&r).len(), 3, "nothing was taken or pruned");
    }

    #[cfg(unix)]
    #[test]
    fn a_delete_that_fails_keeps_the_schedule() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path, m| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(3)).unwrap();
        let due = next_run(&r);
        // Queued just before the delete, which cannot move the collection away, so the
        // schedule goes back into service and the queued run is let go.
        let queued = r.schedules().take_due(due);
        mode(&dir.path().join("collections"), 0o555);
        let deleted = r.delete("a");
        mode(&dir.path().join("collections"), 0o755);
        assert!(deleted.is_err());
        assert_eq!(r.schedule("a").unwrap().keep, 3);
        assert!(!r.schedule("a").unwrap().running);
        assert!(r.run_schedule("a", queued[0].1).snapshot.is_err());
        r.run_due_schedules(due);
        assert_eq!(names(&r).len(), 1, "it runs again as a new revision");
        // The schedule cannot be saved, so the collection stays.
        mode(dir.path(), 0o555);
        let deleted = r.delete("a");
        mode(dir.path(), 0o755);
        assert!(deleted.is_err());
        assert!(r.get("a").is_ok());
        assert_eq!(r.schedule("a").unwrap().keep, 3);
        drop(r);
        assert_eq!(open(dir.path()).schedule("a").unwrap().keep, 3);
    }

    #[test]
    fn a_schedule_dropped_at_start_stays_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(1)).unwrap();
        drop(r);
        // As if a crash came between deleting the collection and removing its schedule.
        fs::rename(
            dir.path().join("collections/a"),
            dir.path().join("collections/.deleted-x"),
        )
        .unwrap();
        drop(open(dir.path()));
        let r = with_collection(dir.path());
        assert!(r.schedules().gauges().is_empty());
        drop(r);
        assert!(open(dir.path()).schedules().gauges().is_empty());
    }

    #[test]
    fn snapshots_never_live_inside_the_collections() {
        let dir = tempfile::TempDir::new().unwrap();
        drop(with_collection(dir.path()));
        let inside = dir.path().join("x/../collections/a/backups");
        let options = RegistryOptions {
            snapshots_dir: Some(inside),
            ..RegistryOptions::default()
        };
        assert!(Registry::open(dir.path(), options).is_err());
        assert!(!dir.path().join("collections/a/backups").exists());
        let options = RegistryOptions {
            snapshots_dir: Some(dir.path().to_owned()),
            ..RegistryOptions::default()
        };
        assert!(Registry::open(dir.path(), options).is_err());
        #[cfg(unix)]
        {
            // A symlink reached after a `..` is followed like the system would.
            std::os::unix::fs::symlink(dir.path().join("collections"), dir.path().join("alias"))
                .unwrap();
            let options = RegistryOptions {
                snapshots_dir: Some(dir.path().join("x/../alias/a/backups")),
                ..RegistryOptions::default()
            };
            assert!(Registry::open(dir.path(), options).is_err());
            assert!(!dir.path().join("collections/a/backups").exists());
            assert!(!dir.path().join("x").exists());
        }
        let elsewhere = tempfile::TempDir::new().unwrap();
        let options = RegistryOptions {
            snapshots_dir: Some(elsewhere.path().join("snaps")),
            ..RegistryOptions::default()
        };
        let r = Registry::open(dir.path(), options).unwrap();
        r.take_snapshot("a").unwrap();
        assert_eq!(
            fs::read_dir(elsewhere.path().join("snaps/a"))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn the_scheduler_thread_runs_due_schedules_and_stops() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = with_collection(dir.path());
        r.set_schedule("a", daily(5)).unwrap();
        drop(r);
        set_since(dir.path(), 2 * DAY);
        let r = Arc::new(open(dir.path()));
        let scheduler = Scheduler::start(r.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while r.schedule("a").unwrap().last_snapshot.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the run never happened"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let stopping = std::time::Instant::now();
        drop(scheduler);
        assert!(stopping.elapsed() < Duration::from_secs(5));
        assert_eq!(names(&r).len(), 1);
    }
}
