//! Standard 5-field cron parser and matcher shared by profile validation and the scheduler.

use std::fmt;

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const MAX_LEN: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cron {
    /// Bitmasks per field (minute 0–59, hour 0–23, day 1–31, month 1–12, weekday 0–6 with 0 = Sunday).
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Vixie cron rule: day-of-month and weekday are OR'd only when neither starts with `*`.
    day_any: bool,
    weekday_any: bool,
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
                "unsupported alias @{other} (allowed: @hourly, @daily, @midnight, @weekly, @monthly, @yearly, @annually)"
            ));
        }
    })
}

/// Parses an unsigned ASCII integer or name (rejects signs and whitespace).
fn value(s: &str, names: &[&str], base: u32) -> Option<u32> {
    if !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().ok();
    }
    names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(s))
        .map(|i| i as u32 + base)
}

fn field(
    raw: &str,
    min: u32,
    max: u32,
    names: &[&str],
    base: u32,
    label: &str,
) -> Result<u64, String> {
    let bad = |why: &str| format!("invalid cron {label} field '{raw}': {why}");
    let mut mask = 0u64;
    for part in raw.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                Some(
                    value(s, &[], 0)
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| bad("step after '/' must be an integer >= 1"))?,
                ),
            ),
            None => (part, None),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (Some(a), Some(b)) = (value(a, names, base), value(b, names, base)) else {
                return Err(bad("value must be a number or name"));
            };
            (a, b)
        } else {
            let Some(v) = value(range, names, base) else {
                return Err(bad("value must be a number or name"));
            };
            (v, if step.is_some() { max } else { v }) // `5/15` steps by 15 from 5 through max.
        };
        if lo < min || hi > max || lo > hi {
            return Err(bad(&format!(
                "values must be in {min}..={max} with start <= end"
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
            return Err(format!("cron expression too long (max {MAX_LEN} bytes)"));
        }
        let src = match input.strip_prefix('@') {
            Some(name) => alias_fields(name)?,
            None => input,
        };
        let parts: Vec<&str> = src.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron requires 5 fields 'minute hour day month weekday' (no seconds field; got {}: {input})",
                parts.len()
            ));
        }
        // Treat `?` as `*` in day-of-month and weekday fields.
        let any = |s: &str| if s == "?" { "*" } else { s }.to_owned();
        let (dom, dow) = (any(parts[2]), any(parts[4]));
        let minutes = field(parts[0], 0, 59, &[], 0, "minute")?;
        let hours = field(parts[1], 0, 23, &[], 0, "hour")? as u32;
        let days = field(&dom, 1, 31, &[], 0, "day")? as u32;
        let months = field(parts[3], 1, 12, &MONTHS, 1, "month")? as u16;
        let mut weekdays = field(&dow, 0, 7, &DAYS, 0, "weekday")?;
        weekdays = (weekdays | (weekdays >> 7 & 1)) & 0x7f; // Fold 7 into 0 (both mean Sunday).
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

    /// Normalized 5-field form with aliases expanded and `?` replaced by `*`.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn hours(&self) -> impl Iterator<Item = u32> + '_ {
        set_bits(u64::from(self.hours))
    }

    pub fn minutes(&self) -> impl Iterator<Item = u32> + '_ {
        set_bits(self.minutes)
    }

    /// Matches month (1–12), day (1–31), and weekday (0–6, 0 = Sunday) using Vixie OR/AND rules.
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

    /// Matches local time components (weekday 0 = Sunday).
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
        // Both constrained => OR: 13th of the month or any Friday.
        let c = Cron::parse("0 0 13 * FRI").unwrap();
        assert!(
            c.matches(0, 0, 13, 5, 2) && c.matches(0, 0, 20, 5, 5) && !c.matches(0, 0, 20, 5, 2)
        );
        // Either field starts with `*` => AND.
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
            "month field is checked first"
        );
        assert!(
            !d.day_matches(13, 31, 0) && !d.day_matches(2, 32, 0),
            "out-of-range values must not match"
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
            "exceeds max length"
        );
    }
}
