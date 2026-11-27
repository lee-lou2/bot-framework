//! Randomized differential tests comparing `occurrences` against a minute-by-minute UTC brute-force reference across DST windows.

use super::*;
use chrono::Timelike;

#[derive(Clone, Debug)]
enum Spec {
    Star,
    StarStep(u32),
    List(Vec<u32>),
    Range(u32, u32),
    RangeStep(u32, u32, u32),
}

impl Spec {
    fn text(&self) -> String {
        match self {
            Spec::Star => "*".into(),
            Spec::StarStep(n) => format!("*/{n}"),
            Spec::List(v) => v.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
            Spec::Range(a, b) => format!("{a}-{b}"),
            Spec::RangeStep(a, b, n) => format!("{a}-{b}/{n}"),
        }
    }

    fn values(&self, min: u32, max: u32) -> Vec<u32> {
        match self {
            Spec::Star => (min..=max).collect(),
            Spec::StarStep(n) => (min..=max).step_by(*n as usize).collect(),
            Spec::List(v) => v.clone(),
            Spec::Range(a, b) => (*a..=*b).collect(),
            Spec::RangeStep(a, b, n) => (*a..=*b).step_by(*n as usize).collect(),
        }
    }

    fn starts_with_star(&self) -> bool {
        matches!(self, Spec::Star | Spec::StarStep(_))
    }
}

/// Deterministic xorshift64* PRNG.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

fn gen_spec(r: &mut Rng, min: u32, max: u32, dense: bool) -> Spec {
    let span = (max - min + 1) as u64;
    let pick = |r: &mut Rng| min + r.below(span) as u32;
    match r.below(10) {
        0..=2 => Spec::Star,
        3 => Spec::StarStep(1 + r.below(if dense { span.min(30) } else { span }) as u32),
        4..=6 => {
            let mut v: Vec<u32> = (0..1 + r.below(3)).map(|_| pick(r)).collect();
            v.sort();
            v.dedup();
            Spec::List(v)
        }
        7 | 8 => {
            let a = pick(r);
            Spec::Range(a, a + r.below((max - a + 1) as u64) as u32)
        }
        _ => {
            let a = pick(r);
            Spec::RangeStep(
                a,
                a + r.below((max - a + 1) as u64) as u32,
                1 + r.below(span.min(15)) as u32,
            )
        }
    }
}

/// Brute-force oracle over every UTC minute in `(after, until]`: DST gaps map to zero UTC minutes and folds map to two.
fn reference(
    f: &[Spec; 5],
    tz: Tz,
    after: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    let sets = [
        f[0].values(0, 59),
        f[1].values(0, 23),
        f[2].values(1, 31),
        f[3].values(1, 12),
        f[4].values(0, 6),
    ];
    let any = f[2].starts_with_star() || f[4].starts_with_star();
    let mut t = after.with_second(0).unwrap().with_nanosecond(0).unwrap() + Span::minutes(1);
    let mut out = Vec::new();
    while t <= until {
        let local = t.with_timezone(&tz).naive_local();
        let dom = sets[2].contains(&local.day());
        let dow = sets[4].contains(&local.weekday().num_days_from_sunday());
        let day_ok = if any { dom && dow } else { dom || dow };
        if sets[0].contains(&local.minute())
            && sets[1].contains(&local.hour())
            && sets[3].contains(&local.month())
            && day_ok
        {
            out.push(t);
        }
        t += Span::minutes(1);
    }
    out
}

fn year_start(year: i32) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&format!("{year}-01-01T00:00:00Z"))
        .unwrap()
        .with_timezone(&Utc)
}

fn transitions(tz: Tz, year: i32) -> Vec<DateTime<Utc>> {
    let mut t = year_start(year);
    let end = year_start(year + 1);
    let off = |t: DateTime<Utc>| {
        tz.offset_from_utc_datetime(&t.naive_utc())
            .fix()
            .local_minus_utc()
    };
    let (mut found, mut prev) = (Vec::new(), off(t));
    while t < end {
        t += Span::minutes(30);
        let now = off(t);
        if now != prev {
            found.push(t);
            prev = now;
        }
    }
    found
}

#[test]
fn occurrences_agree_with_a_brute_force_reference_across_dst_windows() {
    // St_Johns uses 2010 because its DST transition occurred at 00:01 that year.
    let zones = [
        ("UTC", 2026),
        ("Asia/Seoul", 2026),
        ("America/New_York", 2026),
        ("Europe/London", 2026),
        ("Australia/Lord_Howe", 2026),
        ("Asia/Beirut", 2026),
        ("America/St_Johns", 2010),
        ("Asia/Kathmandu", 2026),
        ("Pacific/Chatham", 2026),
    ];
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut cases, mut matched, mut folds_or_gaps) = (0usize, 0usize, 0usize);
    for (name, year) in zones {
        let tz: Tz = name.parse().unwrap();
        let moves = transitions(tz, year);
        for case in 0..160 {
            let f = [
                gen_spec(&mut r, 0, 59, true),
                gen_spec(&mut r, 0, 23, true),
                gen_spec(&mut r, 1, 31, true),
                gen_spec(&mut r, 1, 12, true),
                gen_spec(&mut r, 0, 6, true),
            ];
            let text = f.iter().map(Spec::text).collect::<Vec<_>>().join(" ");
            let cron = Cron::parse(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            let anchor = if !moves.is_empty() && r.below(2) == 0 {
                moves[r.below(moves.len() as u64) as usize] - Span::minutes(r.below(20 * 60) as i64)
            } else {
                year_start(year) + Span::minutes(r.below(365 * 24 * 60) as i64)
            };
            let after = anchor + Span::milliseconds(r.below(60_000) as i64);
            let until = after + Span::minutes(60 + r.below(23 * 60) as i64);
            let want = reference(&f, tz, after, until);
            let got: Vec<DateTime<Utc>> = cron
                .occurrences(tz, after.into(), until.into(), usize::MAX)
                .into_iter()
                .map(Into::into)
                .collect();
            assert_eq!(got, want, "{name} #{case} `{text}` ({after} ~ {until})");
            let mid =
                after + Span::minutes(r.below(((until - after).num_minutes() + 1) as u64) as i64);
            let mut split = cron.occurrences(tz, after.into(), mid.into(), usize::MAX);
            split.extend(cron.occurrences(tz, mid.into(), until.into(), usize::MAX));
            assert_eq!(
                split
                    .into_iter()
                    .map(DateTime::<Utc>::from)
                    .collect::<Vec<_>>(),
                want,
                "{name} #{case} split"
            );
            let k = r.below(5) as usize;
            let head: Vec<DateTime<Utc>> = cron
                .occurrences(tz, after.into(), until.into(), k)
                .into_iter()
                .map(Into::into)
                .collect();
            assert_eq!(head, want[..k.min(want.len())], "{name} #{case} limit {k}");
            cases += 1;
            matched += want.len();
            folds_or_gaps += usize::from(
                moves
                    .iter()
                    .any(|m| *m > after - Span::hours(3) && *m <= until + Span::hours(3)),
            );
        }
    }
    assert!(
        matched > 4000,
        "random expressions must produce enough matches: {matched}"
    );
    assert!(
        folds_or_gaps > cases / 5,
        "must cover enough DST transition windows: {folds_or_gaps}/{cases}"
    );
}
