//! Standard 5-field cron occurrence calculation with timezone and DST handling over `bot_core::cron`.
//!
//! DST rule (`docs/reliability.md`): nonexistent local minutes (spring gap) are skipped, and
//! ambiguous local minutes (fall fold) emit both distinct UTC occurrences in ascending UTC order.

use bot_core::cron as core;
use chrono::{
    DateTime, Datelike, Days, Duration as Span, LocalResult, NaiveDate, NaiveDateTime, Offset,
    TimeZone, Utc,
};
use chrono_tz::Tz;
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

/// Leap days can be up to 8 years apart across non-leap century years, so 9 years covers all valid schedules.
const HORIZON_YEARS: i64 = 9;
const MAX_SPAN_YEARS: i64 = 200;
const MAX_LIMIT: usize = 100_000;
/// Clamp search bounds to 1900-01-01..=9999-12-31 so extreme timestamps never panic.
const MIN_SECS: i64 = -2_208_988_800;
const MAX_SECS: i64 = 253_402_300_799;
const FIRST_CHUNK_DAYS: i64 = 1;
const MAX_CHUNK_DAYS: i64 = 32;
/// Sample interval small enough not to miss any real-world UTC offset transition within a chunk.
const PROBE_HOURS: i64 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron(core::Cron);

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

fn to_utc(t: SystemTime) -> DateTime<Utc> {
    let (secs, nanos) = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (
            i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            d.subsec_nanos(),
        ),
        Err(e) => {
            let d = e.duration();
            let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
            match d.subsec_nanos() {
                0 => (-secs, 0),
                n => (-secs - 1, 1_000_000_000 - n),
            }
        }
    };
    let clamped = secs.clamp(MIN_SECS, MAX_SECS);
    DateTime::from_timestamp(clamped, if clamped == secs { nanos } else { 0 })
        .unwrap_or(DateTime::UNIX_EPOCH)
}

impl Cron {
    pub fn parse(input: &str) -> Result<Self, String> {
        core::Cron::parse(input).map(Cron)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        self.0
            .day_matches(d.month(), d.day(), d.weekday().num_days_from_sunday())
    }

    fn any_day(&self, from: NaiveDate, to: NaiveDate) -> bool {
        let mut date = from;
        while date <= to {
            if self.day_matches(date) {
                return true;
            }
            match date.succ_opt() {
                Some(next) => date = next,
                None => break,
            }
        }
        false
    }

    fn local_matches(&self, lo: NaiveDateTime, hi: NaiveDateTime, out: &mut Vec<NaiveDateTime>) {
        let mut date = lo.date();
        while date <= hi.date() {
            if self.day_matches(date) {
                for h in self.0.hours() {
                    for m in self.0.minutes() {
                        if let Some(t) = date.and_hms_opt(h, m, 0)
                            && t >= lo
                            && t <= hi
                        {
                            out.push(t);
                        }
                    }
                }
            }
            match date.succ_opt() {
                Some(next) => date = next,
                None => break,
            }
        }
    }

    fn chunk(&self, tz: Tz, lo: DateTime<Utc>, hi: DateTime<Utc>, out: &mut Vec<DateTime<Utc>>) {
        // Skip empty windows (e.g. leap-day expressions) without timezone probing; UTC offsets are < 1 day.
        if !self.any_day(
            lo.date_naive() - Days::new(1),
            hi.date_naive() + Days::new(1),
        ) {
            return;
        }
        let (mut omin, mut omax) = (i32::MAX, i32::MIN);
        let mut probe = lo;
        loop {
            let offset = tz
                .offset_from_utc_datetime(&probe.naive_utc())
                .fix()
                .local_minus_utc();
            (omin, omax) = (omin.min(offset), omax.max(offset));
            if probe >= hi {
                break;
            }
            probe = probe
                .checked_add_signed(Span::hours(PROBE_HOURS))
                .map_or(hi, |p| p.min(hi));
        }
        let slack = Span::minutes(1);
        let lo_local = lo.naive_utc() + Span::seconds(i64::from(omin)) - slack;
        let hi_local = hi.naive_utc() + Span::seconds(i64::from(omax)) + slack;
        let mut locals = Vec::new();
        self.local_matches(lo_local, hi_local, &mut locals);
        let mark = out.len();
        let mut keep = |t: DateTime<Tz>| {
            let t = t.with_timezone(&Utc);
            if t > lo && t <= hi {
                out.push(t);
            }
        };
        for local in locals {
            match tz.from_local_datetime(&local) {
                LocalResult::Single(t) => keep(t),
                LocalResult::Ambiguous(first, second) => {
                    keep(first);
                    keep(second);
                }
                LocalResult::None => {}
            }
        }
        out[mark..].sort();
    }

    /// Returns up to `limit` occurrences in `(after, until]` in ascending UTC order, capped by `MAX_SPAN_YEARS` and `MAX_LIMIT`.
    pub fn occurrences(
        &self,
        tz: Tz,
        after: SystemTime,
        until: SystemTime,
        limit: usize,
    ) -> Vec<SystemTime> {
        let limit = limit.min(MAX_LIMIT);
        let (after, until) = (to_utc(after), to_utc(until));
        let end = after
            .checked_add_signed(Span::days(365 * MAX_SPAN_YEARS))
            .map_or(until, |cap| until.min(cap));
        let mut found = Vec::new();
        let mut lo = after;
        let mut chunk = Span::days(FIRST_CHUNK_DAYS);
        while lo < end && found.len() < limit {
            let hi = lo.checked_add_signed(chunk).map_or(end, |h| h.min(end));
            self.chunk(tz, lo, hi, &mut found);
            lo = hi;
            chunk = (chunk * 2).min(Span::days(MAX_CHUNK_DAYS));
        }
        found.truncate(limit);
        found.into_iter().map(SystemTime::from).collect()
    }

    /// Returns the latest occurrence in `(after, until]`, scanning backwards so deep backlogs never hit `MAX_LIMIT`.
    pub fn last_in(&self, tz: Tz, after: SystemTime, until: SystemTime) -> Option<SystemTime> {
        let (mut hi, mut span) = (until, Span::days(FIRST_CHUNK_DAYS));
        // One chunk (<= MAX_CHUNK_DAYS) has at most ~46k minutes, well below MAX_LIMIT.
        while hi > after {
            let lo = SystemTime::from(to_utc(hi) - span).max(after);
            if let Some(found) = self.occurrences(tz, lo, hi, MAX_LIMIT).pop() {
                return Some(found);
            }
            hi = lo;
            span = (span * 2).min(Span::days(MAX_CHUNK_DAYS));
        }
        None
    }

    pub fn next_after(&self, tz: Tz, after: SystemTime) -> Option<SystemTime> {
        let horizon = to_utc(after).checked_add_signed(Span::days(366 * HORIZON_YEARS))?;
        self.occurrences(tz, after, horizon.into(), 1)
            .into_iter()
            .next()
    }
}

#[cfg(test)]
mod tests;
