//! The background optimizer: one thread per shard calling [`Shard::optimize_once`] until
//! there is nothing left to do, then waiting [`OptimizerOptions::interval`] before looking
//! again.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::shard::{OptimizerOptions, Shard};

/// Runs the optimizer for one shard until dropped. Dropping it waits for a job in progress
/// to finish.
pub struct Optimizer {
    stop: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl Optimizer {
    pub fn start(shard: Arc<Shard>, options: OptimizerOptions) -> Optimizer {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("nocturnum-optimizer".into())
            .spawn(move || {
                let (lock, wake) = &*flag;
                loop {
                    if *lock.lock().unwrap_or_else(|e| e.into_inner()) {
                        return;
                    }
                    // A job done means there may be more, so it looks again straight away.
                    // Failures are kept in the shard's optimizer status.
                    if let Ok(Some(_)) = shard.optimize_once(&options) {
                        continue;
                    }
                    let stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = wake
                        .wait_timeout_while(stopped, options.interval, |s| !*s)
                        .unwrap_or_else(|e| e.into_inner());
                }
            })
            .expect("the optimizer thread starts");
        Optimizer {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Optimizer {
    fn drop(&mut self) {
        let (lock, wake) = &*self.stop;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
