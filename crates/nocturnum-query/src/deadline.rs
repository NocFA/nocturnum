//! A time limit on one read, checked as the read works through the memtable and the segments,
//! so a search that would run for minutes stops and says so instead of holding a thread.

use std::time::{Duration, Instant};

use crate::collection::QueryError;

/// When a read has to stop, or never.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    at: Option<Instant>,
    /// The limit it was set from, for the message.
    limit: Duration,
}

/// Loops check the clock once every this many steps, so a tight loop rarely pays for it.
const EVERY: usize = 1024;

impl Deadline {
    /// No limit.
    pub const NONE: Deadline = Deadline {
        at: None,
        limit: Duration::ZERO,
    };

    /// `limit` from now.
    pub fn after(limit: Duration) -> Deadline {
        Deadline::from(Instant::now(), limit)
    }

    /// `limit` from `start`, such as when the request arrived. A limit too far off to
    /// represent is held to about a century, so it never turns into no limit at all.
    pub fn from(start: Instant, limit: Duration) -> Deadline {
        const CENTURY: Duration = Duration::from_secs(100 * 365 * 24 * 3600);
        let at = start
            .checked_add(limit)
            .or_else(|| start.checked_add(CENTURY))
            .unwrap_or(start);
        Deadline {
            at: Some(at),
            limit,
        }
    }

    /// The moment it passes, `None` for no limit.
    pub fn at(&self) -> Option<Instant> {
        self.at
    }

    /// Fails once the deadline has passed.
    pub fn check(&self) -> Result<(), QueryError> {
        match self.at {
            Some(at) if Instant::now() >= at => Err(QueryError::TimedOut(format!(
                "the read ran past its time limit of {} s, a filter or a smaller limit makes it \
                 cheaper",
                self.limit.as_secs_f64()
            ))),
            _ => Ok(()),
        }
    }

    /// [`Deadline::check`] on every [`EVERY`]th step `i` of a loop.
    #[inline]
    pub fn tick(&self, i: usize) -> Result<(), QueryError> {
        if self.at.is_some() && i.is_multiple_of(EVERY) {
            self.check()
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_passes_once() {
        assert!(Deadline::NONE.check().is_ok());
        assert!(Deadline::after(Duration::from_secs(60)).check().is_ok());
        assert!(Deadline::after(Duration::MAX).at().is_some());
        assert!(Deadline::after(Duration::MAX).check().is_ok());
        let past = Deadline::from(
            Instant::now() - Duration::from_secs(2),
            Duration::from_secs(1),
        );
        let err = past.check().unwrap_err();
        assert!(matches!(err, QueryError::TimedOut(_)), "{err}");
        assert!(err.to_string().contains("1 s"));
        // Only every EVERY-th step looks at the clock.
        assert!(past.tick(1).is_ok());
        assert!(past.tick(EVERY).is_err());
    }
}
