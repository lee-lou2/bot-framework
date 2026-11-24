//! 표준 5칸 cron(분 시 일 월 요일)의 해석과 발생 계산. 시간대는 호출마다 받는다(프로필 현지 시각).
//!
//! 문법: `*` · 값 · `a-b` · `a-b/n` · `*/n` · `a/n` · 쉼표 목록, 월(JAN–DEC)·요일(SUN–SAT) 이름, 요일 0과 7은 일요일,
//! `@hourly` `@daily` `@weekly` `@monthly` `@yearly`. 일과 요일을 둘 다 정하면 둘 중 하나만 맞아도 실행한다(Vixie cron과 같다).
//! 둘 중 하나라도 `*`로 시작하면 둘 다 맞아야 한다. 초 칸 · `L` · `W` · `#` 같은 표준 밖 문법은 받지 않는다.
//!
//! DST(docs/reliability.md): 현지 시각에 **없는** 분(봄에 시계가 건너뛴 구간)은 건너뛰고, **두 번 오는** 분(가을에 시계가 되돌아간 구간)은
//! 서로 다른 UTC 발생으로 둘 다 낸다. 발생은 항상 UTC 시각 오름차순이다.

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

/// `next_after`가 앞으로 보는 해 수. 윤일(2월 29일)은 4년마다, 세기 해에는 8년까지 비므로 그 안에 맞는 날이 없으면 없는 것으로 본다.
const HORIZON_YEARS: i64 = 9;
/// `occurrences`가 한 번에 훑는 창의 상한(해). 이보다 긴 창은 뒤쪽을 보지 않는다.
const MAX_SPAN_YEARS: i64 = 200;
/// `occurrences` 한 번이 돌려주는 개수의 상한(메모리 보호). 따라잡기 배치는 이보다 훨씬 작다.
const MAX_LIMIT: usize = 100_000;
/// 훑는 시각의 범위(1900-01-01 ~ 9999-12-31). 밖의 입력은 이 안으로 접는다(패닉하지 않는다).
const MIN_SECS: i64 = -2_208_988_800;
const MAX_SECS: i64 = 253_402_300_799;
/// 창을 이 간격으로 나눠 훑는다. 처음은 하루, 맞는 것이 모자라면 두 배씩 늘려 이 값에서 멈춘다.
const FIRST_CHUNK_DAYS: i64 = 1;
const MAX_CHUNK_DAYS: i64 = 32;
/// 창 안의 UTC 오프셋 변화를 놓치지 않으려는 표본 간격. 실제 시간대의 전환은 이보다 촘촘하지 않다.
const PROBE_HOURS: i64 = 3;

/// 공통 파서·AST(`bot_core::cron::Cron`) 위에 발생 계산만 얹은 것이다. 문법은 `bot-core`가 소유한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron(core::Cron);

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// 시각을 훑는 범위 안으로 접어 UTC 시각으로 바꾼다.
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

    /// `from..=to` 날짜 중 맞는 날이 하나라도 있는가
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

    /// `lo..=hi` 안의 현지 시각(분 단위) 중 맞는 것을 오름차순으로 `out`에 더한다.
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

    /// UTC 구간 `(lo, hi]`의 발생을 오름차순으로 `out`에 더한다.
    /// 구간 안에서 UTC 오프셋이 움직일 수 있는 범위만큼 현지 시각의 후보 범위를 넓혀 후보를 모은 뒤,
    /// 후보마다 UTC로 옮겨(없으면 건너뛰고, 두 번 오면 둘 다) 구간에 드는 것만 남긴다.
    fn chunk(&self, tz: Tz, lo: DateTime<Utc>, hi: DateTime<Utc>, out: &mut Vec<DateTime<Utc>>) {
        // 맞는 날이 하나도 없는 창(윤일처럼 드문 표현식)은 시간대 계산 없이 건너뛴다. UTC 오프셋은 하루보다 작다.
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

    /// `after`(제외)부터 `until`(포함)까지의 발생을 오래된 순서로 최대 `limit`개 돌려준다.
    /// 창이 `MAX_SPAN_YEARS`보다 길면 그만큼만 보고, `limit`은 `MAX_LIMIT`(10만)으로 제한한다. `after >= until`이거나 `limit == 0`이면 빈 목록이다.
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

    /// `after`(제외)부터 `until`(포함)까지의 **가장 늦은** 발생. 구간 끝에서 거슬러 올라가며 찾으므로 발생이 아무리 많이 밀려 있어도
    /// 오래된 것부터 세다가 한도에 걸려 최신보다 이른 것을 고르는 일이 없다. 없으면 None.
    pub fn last_in(&self, tz: Tz, after: SystemTime, until: SystemTime) -> Option<SystemTime> {
        let (mut hi, mut span) = (until, Span::days(FIRST_CHUNK_DAYS));
        // 창 하나(최대 `MAX_CHUNK_DAYS`일)는 발생이 `MAX_LIMIT`개를 넘지 않는다(분 단위가 가장 촘촘하다)
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

    /// `after`(제외) 뒤 가장 이른 발생. `HORIZON_YEARS`년 안에 없으면 None.
    pub fn next_after(&self, tz: Tz, after: SystemTime) -> Option<SystemTime> {
        let horizon = to_utc(after).checked_add_signed(Span::days(366 * HORIZON_YEARS))?;
        self.occurrences(tz, after, horizon.into(), 1)
            .into_iter()
            .next()
    }
}

#[cfg(test)]
mod tests;
