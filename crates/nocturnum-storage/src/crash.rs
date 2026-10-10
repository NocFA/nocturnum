//! Named crash points for the crash tests.
//!
//! With the `crash-points` feature, setting `NOCTURNUM_CRASH_AT=<name>:<n>` makes the process
//! abort the `n`th time it reaches the point called `name`, after running the point's
//! `before` step, which lets a test leave a half-written record or header behind. Without the
//! feature every point compiles to nothing.

#[cfg(feature = "crash-points")]
pub(crate) fn crash_point(name: &str, before: impl FnOnce()) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static TARGET: OnceLock<Option<(String, u64)>> = OnceLock::new();
    static HITS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

    let target = TARGET.get_or_init(|| {
        let spec = std::env::var("NOCTURNUM_CRASH_AT").ok()?;
        let (name, n) = spec.rsplit_once(':')?;
        Some((name.to_owned(), n.parse().ok()?))
    });
    let Some((target, n)) = target else { return };
    if target != name {
        return;
    }
    let mut hits = HITS.lock().unwrap_or_else(|e| e.into_inner());
    let count = hits
        .get_or_insert_default()
        .entry(name.to_owned())
        .or_insert(0);
    *count += 1;
    if *count == *n {
        before();
        die();
    }
}

/// Ends the process the way a crash would, without unwinding or flushing anything.
///
/// SIGKILL rather than `abort`, because macOS hands every aborting process to its crash
/// reporter, which can hold it for many seconds.
#[cfg(feature = "crash-points")]
fn die() -> ! {
    #[cfg(unix)]
    {
        // SAFETY: sending a signal to our own process id has no memory-safety requirements.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    }
    std::process::abort()
}

#[cfg(not(feature = "crash-points"))]
#[inline(always)]
pub(crate) fn crash_point(_name: &str, _before: impl FnOnce()) {}
