//! Five-field cron expressions, `minute hour day-of-month month day-of-week`, evaluated in UTC.
//!
//! Each field takes `*`, a number, a range `a-b`, a step `*/n`, `a-b/n` or `a/n`, and lists of
//! those joined by commas, with month names `jan`..`dec` and weekday names `sun`..`sat`, where
//! weekday 0 and 7 are both Sunday. `@hourly`, `@daily`, `@midnight`, `@weekly`, `@monthly`,
//! `@yearly` and `@annually` stand for the usual expressions.
//!
//! When neither day field starts with `*` a day matching either one runs, as in classic cron,
//! and otherwise a day has to match both, so `0 0 */2 * mon` runs on odd dates that are
//! Mondays and `0 0 1-31 * mon` runs every day.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The longest expression accepted.
pub const MAX_LEN: usize = 256;

/// How far ahead [`Cron::next_after`] looks, one 400-year Gregorian cycle, after which dates
/// and weekdays repeat, so a day the expression names always turns up within it.
const HORIZON_DAYS: i64 = 146_097;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    /// Bits 1 to 31.
    days: u32,
    /// Bits 1 to 12.
    months: u16,
    /// Bits 0 to 6, Sunday first.
    weekdays: u8,
    /// Whether each day field starts with `*`, which decides how the two combine.
    days_star: bool,
    weekdays_star: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

struct Field {
    what: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
    /// The value the first name stands for.
    first_name: u32,
}

const FIELDS: [Field; 5] = [
    Field {
        what: "minute",
        min: 0,
        max: 59,
        names: &[],
        first_name: 0,
    },
    Field {
        what: "hour",
        min: 0,
        max: 23,
        names: &[],
        first_name: 0,
    },
    Field {
        what: "day of month",
        min: 1,
        max: 31,
        names: &[],
        first_name: 0,
    },
    Field {
        what: "month",
        min: 1,
        max: 12,
        names: &MONTHS,
        first_name: 1,
    },
    // 7 is Sunday too, folded onto 0 after parsing.
    Field {
        what: "day of week",
        min: 0,
        max: 7,
        names: &WEEKDAYS,
        first_name: 0,
    },
];

impl Field {
    fn value(&self, s: &str) -> Result<u32, String> {
        let lower = s.to_ascii_lowercase();
        if let Some(i) = self.names.iter().position(|n| *n == lower) {
            return Ok(self.first_name + i as u32);
        }
        let v: u32 = s
            .parse()
            .map_err(|_| format!("{:?} is not a valid {}", s, self.what))?;
        if v < self.min || v > self.max {
            return Err(format!(
                "{} {} is out of range, use {} to {}",
                self.what, v, self.min, self.max
            ));
        }
        Ok(v)
    }

    /// The set of values `spec` names, as a bit mask.
    fn parse(&self, spec: &str) -> Result<u64, String> {
        let mut bits = 0u64;
        for item in spec.split(',') {
            let (range, step) = match item.split_once('/') {
                Some((r, s)) => {
                    let step: u32 =
                        s.parse().ok().filter(|&s| s > 0).ok_or_else(|| {
                            format!("{s:?} is not a valid step in the {}", self.what)
                        })?;
                    (r, Some(step))
                }
                None => (item, None),
            };
            let (lo, hi) = if range == "*" {
                (self.min, self.max)
            } else if let Some((a, b)) = range.split_once('-') {
                let (a, b) = (self.value(a)?, self.value(b)?);
                if a > b {
                    return Err(format!("the {} range {range:?} runs backwards", self.what));
                }
                (a, b)
            } else {
                let a = self.value(range)?;
                // `a/n` runs from `a` to the end of the field.
                (a, if step.is_some() { self.max } else { a })
            };
            let step = step.unwrap_or(1);
            let mut v = Some(lo);
            while let Some(at) = v.filter(|&at| at <= hi) {
                bits |= 1 << at;
                v = at.checked_add(step);
            }
        }
        Ok(bits)
    }
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, String> {
        if expr.len() > MAX_LEN {
            return Err(format!("a cron expression is at most {MAX_LEN} characters"));
        }
        let expr = expr.trim();
        let expanded = match expr.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            _ if expr.starts_with('@') => return Err(format!("unknown cron shortcut {expr:?}")),
            _ => expr,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "a cron expression has 5 fields, minute hour day-of-month month day-of-week, \
                 {expr:?} has {}",
                fields.len()
            ));
        }
        let mut masks = [0u64; 5];
        for ((mask, field), spec) in masks.iter_mut().zip(&FIELDS).zip(&fields) {
            *mask = field.parse(spec)?;
        }
        let weekdays = (masks[4] | masks[4] >> 7) & 0x7f;
        let cron = Cron {
            minutes: masks[0],
            hours: masks[1] as u32,
            days: masks[2] as u32,
            months: masks[3] as u16,
            weekdays: weekdays as u8,
            days_star: fields[2].starts_with('*'),
            weekdays_star: fields[4].starts_with('*'),
        };
        if !cron.can_run() {
            return Err(format!("{expr:?} names a day that never comes"));
        }
        Ok(cron)
    }

    /// Whether some month in the set has some day in the set, such as 31 only in short months,
    /// which only matters when the day of month alone decides.
    fn can_run(&self) -> bool {
        if self.days_star || !self.weekdays_star {
            return true;
        }
        (1..=12).any(|m| {
            self.months & (1 << m) != 0
                && (1..=month_len(2000, m)).any(|d| self.days & (1 << d) != 0)
        })
    }

    fn day_matches(&self, month: u32, day: u32, weekday: u32) -> bool {
        if self.months & (1 << month) == 0 {
            return false;
        }
        let by_day = self.days & (1 << day) != 0;
        let by_weekday = self.weekdays & (1 << weekday) != 0;
        if !self.days_star && !self.weekdays_star {
            by_day || by_weekday
        } else {
            by_day && by_weekday
        }
    }

    /// The first whole minute strictly after `t` that the expression names, `None` only for
    /// an expression [`Cron::parse`] refuses.
    pub fn next_after(&self, t: SystemTime) -> Option<SystemTime> {
        let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let start = secs / 60 + 1;
        let first_day = (start / 1440) as i64;
        let first_minute = (start % 1440) as u32;
        for day in first_day..first_day + HORIZON_DAYS {
            let (_, month, mday) = civil(day);
            let weekday = (day + 4).rem_euclid(7) as u32;
            if !self.day_matches(month, mday, weekday) {
                continue;
            }
            let from = if day == first_day { first_minute } else { 0 };
            for minute_of_day in from..1440 {
                let (h, m) = (minute_of_day / 60, minute_of_day % 60);
                if self.hours & (1 << h) != 0 && self.minutes & (1 << m) != 0 {
                    let at = day as u64 * 86_400 + u64::from(minute_of_day) * 60;
                    return Some(UNIX_EPOCH + Duration::from_secs(at));
                }
            }
        }
        None
    }
}

fn month_len(year: i64, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// The civil date `(year, month, day)` of a day counted from 1970-01-01, after Howard
/// Hinnant's algorithm.
pub(crate) fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::rfc3339;

    /// Parses an RFC 3339 time in UTC to the second.
    fn at(s: &str) -> SystemTime {
        let n = |r: std::ops::Range<usize>| s[r].parse::<i64>().unwrap();
        let (y, mo, d) = (n(0..4), n(5..7) as u32, n(8..10) as u32);
        let days = (1970..y)
            .map(|y| 365 + i64::from(month_len(y, 2) == 29))
            .sum::<i64>()
            + (1..mo).map(|m| i64::from(month_len(y, m))).sum::<i64>()
            + i64::from(d - 1);
        let secs = days * 86_400 + n(11..13) * 3600 + n(14..16) * 60 + n(17..19);
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    }

    fn next(expr: &str, from: &str) -> String {
        rfc3339(Cron::parse(expr).unwrap().next_after(at(from)).unwrap())
    }

    #[test]
    fn finds_the_next_minute() {
        assert_eq!(
            next("0 3 * * *", "2026-10-11T02:59:59Z"),
            "2026-10-11T03:00:00Z"
        );
        assert_eq!(
            next("0 3 * * *", "2026-10-11T03:00:00Z"),
            "2026-10-12T03:00:00Z"
        );
        assert_eq!(
            next("@hourly", "2026-12-31T23:30:00Z"),
            "2027-01-01T00:00:00Z"
        );
        assert_eq!(
            next("*/15 * * * *", "2026-10-11T10:07:00Z"),
            "2026-10-11T10:15:00Z"
        );
        assert_eq!(
            next("5-10/5 1 * * *", "2026-10-11T01:05:00Z"),
            "2026-10-11T01:10:00Z"
        );
        assert_eq!(
            next("30/15 * * * *", "2026-10-11T10:46:00Z"),
            "2026-10-11T11:30:00Z"
        );
        assert_eq!(
            next("0 0 1 jan *", "2026-10-11T00:00:00Z"),
            "2027-01-01T00:00:00Z"
        );
        assert_eq!(
            next("@monthly", "2026-01-31T12:00:00Z"),
            "2026-02-01T00:00:00Z"
        );
    }

    #[test]
    fn weekdays_and_month_days() {
        // 2026-10-11 is a Sunday.
        assert_eq!(
            next("0 9 * * MON-FRI", "2026-10-10T12:00:00Z"),
            "2026-10-12T09:00:00Z"
        );
        assert_eq!(
            next("0 0 * * 7", "2026-10-11T00:00:00Z"),
            "2026-10-18T00:00:00Z"
        );
        assert_eq!(
            next("@weekly", "2026-10-11T00:00:00Z"),
            "2026-10-18T00:00:00Z"
        );
        // Neither day field starts with `*`: the 13th or any Friday.
        assert_eq!(
            next("0 0 13 * fri", "2026-10-11T00:00:00Z"),
            "2026-10-13T00:00:00Z"
        );
        assert_eq!(
            next("0 0 13 * fri", "2026-10-13T00:00:00Z"),
            "2026-10-16T00:00:00Z"
        );
        // One starts with `*`: a day has to match both.
        assert_eq!(
            next("0 0 */2 * mon", "2026-10-11T00:00:00Z"),
            "2026-10-19T00:00:00Z"
        );
        assert_eq!(
            next("0 0 1-31 * mon", "2026-10-11T00:00:00Z"),
            "2026-10-12T00:00:00Z"
        );
        assert_eq!(
            next("0 0 1-31 * mon", "2026-10-12T00:00:00Z"),
            "2026-10-13T00:00:00Z"
        );
        assert_eq!(
            next("0 0 31 2 mon", "2026-10-11T00:00:00Z"),
            "2027-02-01T00:00:00Z"
        );
        // 1 February falls on a Monday in 2027 and next in 2038.
        assert_eq!(
            next("0 0 */31 2 mon", "2027-02-01T00:00:00Z"),
            "2038-02-01T00:00:00Z"
        );
        assert_eq!(
            next("0 0 29 2 *", "2016-02-29T00:00:00Z"),
            "2020-02-29T00:00:00Z"
        );
        assert_eq!(
            next("0 0 31 * *", "2026-10-31T00:00:00Z"),
            "2026-12-31T00:00:00Z"
        );
        assert_eq!(
            next("0 0 29 2 *", "2096-03-01T00:00:00Z"),
            "2104-02-29T00:00:00Z"
        );
    }

    #[test]
    fn refuses_bad_expressions() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "5-1 * * * *",
            "*/0 * * * *",
            "a * * * *",
            "1,,2 * * * *",
            "@sometimes",
            "0 0 30 2 *",
            "0 0 31 4,6,9,11 *",
            &"1,".repeat(200),
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Cron::parse(" 0 3 * * * ").is_ok());
        // A step past the end of the field names only the start.
        assert_eq!(
            next("1/4294967295 * * * *", "2026-10-11T10:00:00Z"),
            "2026-10-11T10:01:00Z"
        );
        assert_eq!(
            next("1/4294967295 * * * *", "2026-10-11T10:01:00Z"),
            "2026-10-11T11:01:00Z"
        );
        assert!(Cron::parse("0 0 31 2,3 *").is_ok());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(-1), (1969, 12, 31));
    }
}
