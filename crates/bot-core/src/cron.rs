//! 표준 5칸 cron(분 시 일 월 요일)의 문법과 "이 값이 맞는가". 프로필 검증(bot-profile)과 스케줄러(bot-runtime)가
//! 같은 파서를 쓴다. 다음 발생 시각 계산·시간대·DST는 스케줄러 몫이며, 여기에는 시간 의존이 없다.
//!
//! 문법: `*` · 값 · `a-b` · `a-b/n` · `*/n` · `a/n` · 쉼표 목록, 월(JAN–DEC)·요일(SUN–SAT) 이름, 요일 0과 7은 일요일,
//! 별칭 `@hourly` `@daily` `@midnight` `@weekly` `@monthly` `@yearly` `@annually`, 일·요일 칸의 `?`(= `*`).
//! 일과 요일을 둘 다 정하면 둘 중 하나만 맞아도 실행한다(Vixie cron). 둘 중 하나라도 `*`로 시작하면 둘 다 맞아야 한다.
//! 초 칸·`L`·`W`·`#` 같은 표준 밖 문법은 받지 않는다. `as_str()`은 별칭을 풀고 `?`를 `*`로 바꾼 정규 5칸 표현이다.
use std::fmt;

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
/// 입력 길이 상한(별칭 포함). 5칸 표현은 이보다 훨씬 짧다.
const MAX_LEN: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cron {
    /// 맞는 값마다 비트 하나(분 0–59 · 시 0–23 · 일 1–31 · 월 1–12 · 요일 0–6, 0이 일요일)
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// 일·요일 칸이 `*`로 시작하는가 — 둘 다 정했을 때만 OR이다
    day_any: bool,
    weekday_any: bool,
    /// 공백을 하나로 정리한 정규 표현식
    text: String,
}

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn alias_fields(name: &str) -> Result<&'static str, String> {
    Ok(match name {
        "yearly" | "annually" => "0 0 1 1 *",
        "monthly" => "0 0 1 * *",
        "weekly" => "0 0 * * 0",
        "daily" | "midnight" => "0 0 * * *",
        "hourly" => "0 * * * *",
        other => {
            return Err(format!(
                "@{other}는 지원하지 않아요 (@hourly · @daily · @midnight · @weekly · @monthly · @yearly · @annually만 써요)"
            ));
        }
    })
}

/// 숫자 하나 또는 이름 → 값. 부호·공백 같은 변칙은 받지 않는다.
fn value(s: &str, names: &[&str], base: u32) -> Option<u32> {
    if !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().ok();
    }
    names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(s))
        .map(|i| i as u32 + base)
}

/// 한 칸 → 맞는 값의 비트 집합
fn field(
    raw: &str,
    min: u32,
    max: u32,
    names: &[&str],
    base: u32,
    label: &str,
) -> Result<u64, String> {
    let bad = |why: &str| format!("cron {label} 칸 '{raw}'이 잘못됐어요: {why}");
    let mut mask = 0u64;
    for part in raw.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                Some(
                    value(s, &[], 0)
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| bad("'/' 뒤는 1 이상의 숫자여야 해요"))?,
                ),
            ),
            None => (part, None),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (Some(a), Some(b)) = (value(a, names, base), value(b, names, base)) else {
                return Err(bad("값은 숫자나 이름이어야 해요"));
            };
            (a, b)
        } else {
            let Some(v) = value(range, names, base) else {
                return Err(bad("값은 숫자나 이름이어야 해요"));
            };
            (v, if step.is_some() { max } else { v }) // `5/15`는 5부터 끝까지 15씩
        };
        if lo < min || hi > max || lo > hi {
            return Err(bad(&format!(
                "{min}~{max} 안의 값이고 시작이 끝보다 크지 않아야 해요"
            )));
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v += step.unwrap_or(1);
        }
    }
    Ok(mask)
}

/// 켜진 비트의 위치를 오름차순으로
fn set_bits(mut mask: u64) -> impl Iterator<Item = u32> {
    std::iter::from_fn(move || {
        (mask != 0).then(|| {
            let bit = mask.trailing_zeros();
            mask &= mask - 1;
            bit
        })
    })
}

impl Cron {
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.len() > MAX_LEN {
            return Err(format!("cron 표현식이 너무 길어요(최대 {MAX_LEN}자)"));
        }
        let src = match input.strip_prefix('@') {
            Some(name) => alias_fields(name)?,
            None => input,
        };
        let parts: Vec<&str> = src.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron은 '분 시 일 월 요일' 다섯 칸이어야 해요 — 초 칸은 없어요 (받은 값은 {}칸: {input})",
                parts.len()
            ));
        }
        // 일·요일 칸의 `?`는 `*`와 같게 받는다
        let any = |s: &str| if s == "?" { "*" } else { s }.to_owned();
        let (dom, dow) = (any(parts[2]), any(parts[4]));
        let minutes = field(parts[0], 0, 59, &[], 0, "분")?;
        let hours = field(parts[1], 0, 23, &[], 0, "시")? as u32;
        let days = field(&dom, 1, 31, &[], 0, "일")? as u32;
        let months = field(parts[3], 1, 12, &MONTHS, 1, "월")? as u16;
        let mut weekdays = field(&dow, 0, 7, &DAYS, 0, "요일")?;
        weekdays = (weekdays | (weekdays >> 7 & 1)) & 0x7f; // 7도 일요일
        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays: weekdays as u8,
            day_any: dom.starts_with('*'),
            weekday_any: dow.starts_with('*'),
            text: format!("{} {} {} {} {}", parts[0], parts[1], dom, parts[3], dow),
        })
    }

    /// 별칭을 풀고 `?`를 `*`로 바꾼 정규 5칸 표현
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// 허용하는 시(0–23)를 오름차순으로
    pub fn hours(&self) -> impl Iterator<Item = u32> + '_ {
        set_bits(u64::from(self.hours))
    }

    /// 허용하는 분(0–59)을 오름차순으로
    pub fn minutes(&self) -> impl Iterator<Item = u32> + '_ {
        set_bits(self.minutes)
    }

    /// 그 달(1–12)과 날(일 1–31, 요일 0–6·0이 일요일)이 맞는가. 일·요일 규칙(OR/AND)을 적용한다.
    pub fn day_matches(&self, month: u32, day: u32, weekday: u32) -> bool {
        let bit = |mask: u64, n: u32| n < 64 && mask >> n & 1 == 1;
        if !bit(u64::from(self.months), month) {
            return false;
        }
        let dom = bit(u64::from(self.days), day);
        let dow = bit(u64::from(self.weekdays), weekday);
        if self.day_any || self.weekday_any {
            dom && dow
        } else {
            dom || dow
        }
    }

    /// 프로필 현지 시각의 분·시·일·월·요일(0=일요일)이 이 표현식과 맞는가.
    pub fn matches(&self, minute: u32, hour: u32, day: u32, month: u32, weekday: u32) -> bool {
        let bit = |mask: u64, n: u32| n < 64 && mask >> n & 1 == 1;
        bit(self.minutes, minute)
            && bit(u64::from(self.hours), hour)
            && self.day_matches(month, day, weekday)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches() {
        let every = Cron::parse("* * * * *").unwrap();
        assert!(every.matches(0, 0, 1, 1, 0) && every.matches(59, 23, 31, 12, 6));
        let c = Cron::parse("*/15 9-17 * JAN-MAR MON-FRI").unwrap();
        assert!(c.matches(30, 9, 5, 2, 1) && !c.matches(31, 9, 5, 2, 1));
        assert!(
            !c.matches(30, 18, 5, 2, 1) && !c.matches(30, 9, 5, 4, 1) && !c.matches(30, 9, 5, 2, 0)
        );
        let (seven, zero) = (
            Cron::parse("0 0 * * 7").unwrap(),
            Cron::parse("0 0 * * 0").unwrap(),
        );
        assert!((0..7).all(|d| seven.matches(0, 0, 1, 1, d) == zero.matches(0, 0, 1, 1, d)));
        assert!(seven.matches(0, 0, 1, 1, 0) && !seven.matches(0, 0, 1, 1, 1));
        assert!(Cron::parse("5/20 * * * *").unwrap().matches(45, 0, 1, 1, 0));
        assert_eq!(
            Cron::parse("  0   9 * * *  ").unwrap().as_str(),
            "0 9 * * *"
        );
    }

    #[test]
    fn day_of_month_or_weekday() {
        // 둘 다 정하면 OR: 매월 13일이거나 금요일
        let c = Cron::parse("0 0 13 * FRI").unwrap();
        assert!(
            c.matches(0, 0, 13, 5, 2) && c.matches(0, 0, 20, 5, 5) && !c.matches(0, 0, 20, 5, 2)
        );
        // 한쪽이 *이면 AND
        let d = Cron::parse("0 0 13 * *").unwrap();
        assert!(d.matches(0, 0, 13, 5, 2) && !d.matches(0, 0, 14, 5, 2));
        let e = Cron::parse("0 0 * * FRI").unwrap();
        assert!(e.matches(0, 0, 14, 5, 5) && !e.matches(0, 0, 14, 5, 4));
    }

    #[test]
    fn aliases_and_question_marks_are_normalized() {
        for (alias, five) in [
            ("@hourly", "0 * * * *"),
            ("@daily", "0 0 * * *"),
            ("@midnight", "0 0 * * *"),
            ("@weekly", "0 0 * * 0"),
            ("@monthly", "0 0 1 * *"),
            ("@yearly", "0 0 1 1 *"),
            ("@annually", "0 0 1 1 *"),
        ] {
            let (a, b) = (Cron::parse(alias).unwrap(), Cron::parse(five).unwrap());
            assert_eq!(a, b, "{alias}");
            assert_eq!(a.as_str(), five);
        }
        let q = Cron::parse("0 9 ? * MON").unwrap();
        assert_eq!(q, Cron::parse("0 9 * * MON").unwrap());
        assert_eq!(q.as_str(), "0 9 * * MON");
        assert_eq!(
            Cron::parse("0 9 1 * ?").unwrap(),
            Cron::parse("0 9 1 * *").unwrap()
        );
        assert_eq!(Cron::parse(" @daily ").unwrap().to_string(), "0 0 * * *");
    }

    #[test]
    fn exposes_allowed_values_in_order() {
        let c = Cron::parse("5,30 8-10 * * *").unwrap();
        assert_eq!(c.minutes().collect::<Vec<_>>(), [5, 30]);
        assert_eq!(c.hours().collect::<Vec<_>>(), [8, 9, 10]);
        assert_eq!(Cron::parse("* * * * *").unwrap().minutes().count(), 60);
        assert_eq!(Cron::parse("* * * * *").unwrap().hours().count(), 24);
        let d = Cron::parse("0 0 31 2,4 *").unwrap();
        assert!(
            d.day_matches(2, 31, 0) && !d.day_matches(3, 31, 0),
            "월 칸이 먼저 걸러진다"
        );
        assert!(
            !d.day_matches(13, 31, 0) && !d.day_matches(2, 32, 0),
            "범위 밖은 맞지 않는다"
        );
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "@reboot",
            "@daily extra",
            "a * * * *",
            "-1 * * * *",
            "1,,2 * * * *",
            "L * * * *",
            "? * * * *",
            "* ? * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
        assert!(
            Cron::parse(&format!("{} * * * *", "1,".repeat(200))).is_err(),
            "길이 상한"
        );
    }
}
