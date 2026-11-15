//! Calendar schedules (issue #103): a five-field cron expression
//! (`minute hour day-of-month month day-of-week`) evaluated in an IANA time
//! zone, so "every morning at 8" stays at 8 across daylight-saving changes.
//!
//! Fields take `*`, numbers, ranges `a-b`, lists `a,b` and steps `*/n` or
//! `a-b/n`; months and weekdays also take names (`jan`, `mon`), and weekday
//! 7 is Sunday like 0. When both day fields are restricted, a day matching
//! either runs, as in Vixie cron. A local time skipped by a DST change does
//! not run that day; one repeated by it runs once, the first time.

use chrono::{DateTime, Datelike as _, Days, LocalResult, NaiveDate, TimeZone as _, Utc};
use chrono_tz::Tz;

/// Days searched ahead for the next run: four years and a day, enough for
/// February 29.
const SEARCH_DAYS: u64 = 4 * 366 + 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    zone: Tz,
    expr: String,
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    any_day: bool,
    any_weekday: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Calendar {
    /// `expr` in the zone named `zone`.
    pub fn parse(zone: &str, expr: &str) -> Result<Self, String> {
        let zone: Tz = zone
            .parse()
            .map_err(|_| format!("{zone:?} is not an IANA time zone"))?;
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!(
                "{expr:?} needs five fields: minute hour day-of-month month day-of-week"
            ));
        };
        let weekdays = field(weekday, 0, 7, &WEEKDAYS, 0, "day-of-week")?;
        // 7 is Sunday too.
        let weekdays = (weekdays | (weekdays >> 7)) & 0x7f;
        Ok(Self {
            zone,
            expr: fields.join(" "),
            minutes: field(minute, 0, 59, &[], 0, "minute")?,
            hours: u32::try_from(field(hour, 0, 23, &[], 0, "hour")?).unwrap_or(0),
            days: u32::try_from(field(day, 1, 31, &[], 0, "day-of-month")?).unwrap_or(0),
            months: u16::try_from(field(month, 1, 12, &MONTHS, 1, "month")?).unwrap_or(0),
            weekdays: u8::try_from(weekdays).unwrap_or(0),
            any_day: day == "*",
            any_weekday: weekday == "*",
        })
    }

    /// A schedule written in words, as a cron expression: `daily 08:00`,
    /// `weekdays 09:30`, `weekends 10:00`, `weekly mon 10:00`,
    /// `monthly 1 09:00`, `hourly :15`. `None` when `text` is not one.
    #[must_use]
    pub fn from_words(text: &str) -> Option<String> {
        let words: Vec<String> = text
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let time = |s: &str| -> Option<(u32, u32)> {
            let (h, m) = s.split_once(':')?;
            let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
            (h < 24 && m < 60).then_some((h, m))
        };
        match words.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["hourly", at] => {
                let m: u32 = at.strip_prefix(':')?.parse().ok()?;
                (m < 60).then(|| format!("{m} * * * *"))
            }
            ["daily", t] => time(t).map(|(h, m)| format!("{m} {h} * * *")),
            ["weekdays", t] => time(t).map(|(h, m)| format!("{m} {h} * * 1-5")),
            ["weekends", t] => time(t).map(|(h, m)| format!("{m} {h} * * 0,6")),
            ["weekly", day, t] => {
                let d = WEEKDAYS
                    .iter()
                    .position(|w| day.starts_with(w))
                    .filter(|_| day.len() >= 3)?;
                time(t).map(|(h, m)| format!("{m} {h} * * {d}"))
            }
            ["monthly", day, t] => {
                let d: u32 = day.parse().ok().filter(|d| (1..=31).contains(d))?;
                time(t).map(|(h, m)| format!("{m} {h} {d} * *"))
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn zone(&self) -> &str {
        self.zone.name()
    }

    #[must_use]
    pub fn expr(&self) -> &str {
        &self.expr
    }

    /// The first run strictly after `after`, if there is one within four
    /// years.
    #[must_use]
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = after.with_timezone(&self.zone).date_naive();
        for offset in 0..SEARCH_DAYS {
            let date = start.checked_add_days(Days::new(offset))?;
            if !self.runs_on(date) {
                continue;
            }
            for hour in (0_u32..24).filter(|h| self.hours & (1_u32 << h) != 0) {
                for minute in (0_u32..60).filter(|m| self.minutes & (1_u64 << m) != 0) {
                    let Some(local) = date.and_hms_opt(hour, minute, 0) else {
                        continue;
                    };
                    let at = match self.zone.from_local_datetime(&local) {
                        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t,
                        LocalResult::None => continue,
                    };
                    let at = at.with_timezone(&Utc);
                    if at > after {
                        return Some(at);
                    }
                }
            }
        }
        None
    }

    fn runs_on(&self, date: NaiveDate) -> bool {
        if self.months & (1_u16 << date.month()) == 0 {
            return false;
        }
        let day = self.days & (1_u32 << date.day()) != 0;
        let weekday = self.weekdays & (1_u8 << date.weekday().num_days_from_sunday()) != 0;
        match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (false, true) => day,
            (true, false) => weekday,
            (false, false) => day || weekday,
        }
    }
}

/// The values one field allows, as bits; `names[i]` stands for
/// `i + name_base`.
fn field(
    text: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_base: u32,
    what: &str,
) -> Result<u64, String> {
    let bad = || format!("{what} field {text:?} is not valid");
    let value = |s: &str| -> Result<u32, String> {
        let lower = s.to_ascii_lowercase();
        let n = match names.iter().position(|n| *n == lower) {
            Some(i) => u32::try_from(i).map_err(|_| bad())? + name_base,
            None => s.parse().map_err(|_| bad())?,
        };
        if (min..=max).contains(&n) {
            Ok(n)
        } else {
            Err(format!("{what} {n} is outside {min}-{max}"))
        }
    };
    let mut bits = 0_u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(bad)?),
            None => (part, 1),
        };
        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            (v, if part.contains('/') { max } else { v })
        };
        if from > to {
            return Err(bad());
        }
        for n in (from..=to).step_by(step as usize) {
            bits |= 1_u64 << n;
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn daily_runs_stay_at_local_time_across_dst() {
        let c = Calendar::parse("Europe/Berlin", "0 8 * * *").unwrap();
        // Winter: UTC+1.
        assert_eq!(
            c.next_after(utc("2026-03-27T12:00:00Z")),
            Some(utc("2026-03-28T07:00:00Z"))
        );
        // Summer time starts 2026-03-29: UTC+2.
        assert_eq!(
            c.next_after(utc("2026-03-28T07:00:00Z")),
            Some(utc("2026-03-29T06:00:00Z"))
        );
    }

    #[test]
    fn skipped_and_repeated_local_times() {
        let gap = Calendar::parse("Europe/Berlin", "30 2 * * *").unwrap();
        // 02:30 does not exist on 2026-03-29; the next run is the day after.
        assert_eq!(
            gap.next_after(utc("2026-03-28T02:00:00Z")),
            Some(utc("2026-03-30T00:30:00Z"))
        );
        let repeat = Calendar::parse("Europe/Berlin", "30 2 * * *").unwrap();
        // 02:30 happens twice on 2026-10-25; it runs the first time only.
        let first = repeat.next_after(utc("2026-10-24T12:00:00Z")).unwrap();
        assert_eq!(first, utc("2026-10-25T00:30:00Z"));
        assert_eq!(repeat.next_after(first), Some(utc("2026-10-26T01:30:00Z")));
    }

    #[test]
    fn fields_and_names() {
        let c = Calendar::parse("Asia/Shanghai", "*/15 9-17 * * mon-fri").unwrap();
        // Saturday 2026-11-14 10:00 local -> Monday 09:00 local (01:00 UTC).
        assert_eq!(
            c.next_after(utc("2026-11-14T02:00:00Z")),
            Some(utc("2026-11-16T01:00:00Z"))
        );
        assert_eq!(
            c.next_after(utc("2026-11-16T01:00:00Z")),
            Some(utc("2026-11-16T01:15:00Z"))
        );
        let sunday = Calendar::parse("UTC", "0 12 * * 7").unwrap();
        assert_eq!(
            sunday.next_after(utc("2026-11-15T11:00:00Z")),
            Some(utc("2026-11-15T12:00:00Z"))
        );
        let leap = Calendar::parse("UTC", "0 0 29 feb *").unwrap();
        assert_eq!(
            leap.next_after(utc("2026-03-01T00:00:00Z")),
            Some(utc("2028-02-29T00:00:00Z"))
        );
        let either = Calendar::parse("UTC", "0 0 1 * mon").unwrap();
        // The 1st, or any Monday: Monday 2026-11-16 comes first.
        assert_eq!(
            either.next_after(utc("2026-11-15T00:00:00Z")),
            Some(utc("2026-11-16T00:00:00Z"))
        );
        assert!(
            Calendar::parse("UTC", "0 0 30 feb *")
                .unwrap()
                .next_after(utc("2026-01-01T00:00:00Z"))
                .is_none()
        );
    }

    #[test]
    fn bad_expressions_are_refused() {
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "5-1 * * * *",
            "*/0 * * * *",
            "x * * * *",
        ] {
            assert!(Calendar::parse("UTC", bad).is_err(), "{bad:?}");
        }
        assert!(Calendar::parse("Mars/Base", "* * * * *").is_err());
    }

    #[test]
    fn schedules_in_words() {
        assert_eq!(
            Calendar::from_words("daily 08:00").as_deref(),
            Some("0 8 * * *")
        );
        assert_eq!(
            Calendar::from_words("Weekdays 9:30").as_deref(),
            Some("30 9 * * 1-5")
        );
        assert_eq!(
            Calendar::from_words("weekends 10:05").as_deref(),
            Some("5 10 * * 0,6")
        );
        assert_eq!(
            Calendar::from_words("weekly monday 10:00").as_deref(),
            Some("0 10 * * 1")
        );
        assert_eq!(
            Calendar::from_words("monthly 1 09:00").as_deref(),
            Some("0 9 1 * *")
        );
        assert_eq!(
            Calendar::from_words("hourly :15").as_deref(),
            Some("15 * * * *")
        );
        assert_eq!(Calendar::from_words("daily 25:00"), None);
        assert_eq!(Calendar::from_words("weekly mo 10:00"), None);
        assert_eq!(Calendar::from_words("0 8 * * *"), None);
    }
}
