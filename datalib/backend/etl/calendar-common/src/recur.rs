//! An RFC 5545 `RRULE` run forward: the wall-clock times a rule gives
//! from a series' first start, read in the series' own zone so a weekly
//! 9am stays 9am across daylight saving. Zones, `UNTIL` and the dates a
//! series no longer has are [`crate::occurrences`]'s; this file only
//! counts days. A rule part it cannot follow is refused by name rather
//! than skipped, since skipping one changes which dates come out.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};

/// No series is followed past this year: a rule that matches no date at
/// all (`BYMONTH=2;BYMONTHDAY=30`) would otherwise run forever.
const LAST_YEAR: i32 = 2200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// `UNTIL`, kept in the form it was written: RFC 5545 has it in UTC
/// beside a zoned start, a date beside an all-day one, and wall-clock
/// time beside a floating one, and clients write a date beside a zoned
/// start too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    Date(NaiveDate),
    Utc(NaiveDateTime),
    Local(NaiveDateTime),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub freq: Freq,
    pub interval: u32,
    pub count: Option<u32>,
    pub until: Option<Until>,
    by_month: Vec<u32>,
    by_month_day: Vec<i32>,
    by_year_day: Vec<i32>,
    by_day: Vec<(Option<i32>, Weekday)>,
    by_hour: Vec<u32>,
    by_minute: Vec<u32>,
    by_second: Vec<u32>,
    by_set_pos: Vec<i32>,
    wkst: Weekday,
}

impl Rule {
    /// An `RRULE` value (`FREQ=WEEKLY;BYDAY=MO`), with or without its
    /// `RRULE:` name. `Err` names the part it cannot follow.
    pub fn parse(rule: &str) -> Result<Rule, String> {
        let mut freq = None;
        let mut r = Rule {
            freq: Freq::Daily,
            interval: 1,
            count: None,
            until: None,
            by_month: Vec::new(),
            by_month_day: Vec::new(),
            by_year_day: Vec::new(),
            by_day: Vec::new(),
            by_hour: Vec::new(),
            by_minute: Vec::new(),
            by_second: Vec::new(),
            by_set_pos: Vec::new(),
            wkst: Weekday::Mon,
        };
        let body = rule.trim();
        let body = body.strip_prefix("RRULE:").unwrap_or(body);
        for part in body.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let bad = || format!("cannot follow `{part}`");
            let (k, v) = part.split_once('=').ok_or_else(bad)?;
            let v = v.trim();
            match k.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match v.to_ascii_uppercase().as_str() {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        _ => return Err(bad()),
                    })
                }
                "INTERVAL" => r.interval = v.parse().ok().filter(|n| *n >= 1).ok_or_else(bad)?,
                "COUNT" => r.count = Some(v.parse().map_err(|_| bad())?),
                "UNTIL" => r.until = Some(until(v).ok_or_else(bad)?),
                "BYMONTH" => r.by_month = numbers(v, 1, 12, false).ok_or_else(bad)?,
                "BYMONTHDAY" => r.by_month_day = numbers(v, 1, 31, true).ok_or_else(bad)?,
                "BYYEARDAY" => r.by_year_day = numbers(v, 1, 366, true).ok_or_else(bad)?,
                "BYSETPOS" => r.by_set_pos = numbers(v, 1, 366, true).ok_or_else(bad)?,
                "BYHOUR" => r.by_hour = numbers(v, 0, 23, false).ok_or_else(bad)?,
                "BYMINUTE" => r.by_minute = numbers(v, 0, 59, false).ok_or_else(bad)?,
                "BYSECOND" => r.by_second = numbers(v, 0, 59, false).ok_or_else(bad)?,
                "BYDAY" => {
                    r.by_day = v
                        .split(',')
                        .map(|d| by_day(d.trim()))
                        .collect::<Option<_>>()
                        .ok_or_else(bad)?
                }
                "WKST" => r.wkst = weekday(v).ok_or_else(bad)?,
                _ => return Err(bad()),
            }
        }
        r.freq = freq.ok_or_else(|| format!("no FREQ in `{rule}`"))?;
        if r.count.is_some() && r.until.is_some() {
            return Err(format!("both COUNT and UNTIL in `{rule}`"));
        }
        Ok(r)
    }

    /// The wall-clock starts this rule gives a series that first starts
    /// at `first`, in order, `first` itself included (RFC 5545 counts
    /// it as the first occurrence whether or not the rule gives it).
    /// `COUNT` is applied here; `UNTIL` is the caller's, since comparing
    /// a UTC one needs the zone.
    pub fn starts(&self, first: NaiveDateTime) -> Starts {
        Starts {
            rule: self.with_defaults(first),
            first,
            period: 0,
            pending: Vec::new(),
            emitted: 0,
            first_done: false,
            exhausted: false,
        }
    }

    /// The parts RFC 5545 takes from `DTSTART` when the rule leaves them
    /// out: a weekly rule repeats on its first start's weekday, a monthly
    /// one on its day of the month, a yearly one on its date.
    fn with_defaults(&self, first: NaiveDateTime) -> Rule {
        let mut r = self.clone();
        let no_day = r.by_day.is_empty() && r.by_month_day.is_empty() && r.by_year_day.is_empty();
        if no_day {
            match r.freq {
                Freq::Daily => {}
                Freq::Weekly => r.by_day = vec![(None, first.weekday())],
                Freq::Monthly => r.by_month_day = vec![first.day() as i32],
                Freq::Yearly => {
                    r.by_month_day = vec![first.day() as i32];
                    if r.by_month.is_empty() {
                        r.by_month = vec![first.month()];
                    }
                }
            }
        }
        // An ordinal on BYDAY only means something within a month or a year.
        if matches!(r.freq, Freq::Daily | Freq::Weekly) {
            for d in &mut r.by_day {
                d.0 = None;
            }
        }
        for (list, default) in [
            (&mut r.by_hour, first.hour()),
            (&mut r.by_minute, first.minute()),
            (&mut r.by_second, first.second()),
        ] {
            if list.is_empty() {
                list.push(default);
            }
            list.sort_unstable();
            list.dedup();
        }
        r
    }

    fn period_days(&self, first: NaiveDate, k: i64) -> Option<Vec<NaiveDate>> {
        let step = k * i64::from(self.interval);
        let days = match self.freq {
            Freq::Daily => vec![first.checked_add_signed(Duration::days(step))?],
            Freq::Weekly => {
                let back = (7 + first.weekday().num_days_from_monday() as i64
                    - self.wkst.num_days_from_monday() as i64)
                    % 7;
                let start = first.checked_add_signed(Duration::days(step * 7 - back))?;
                (0..7).map(|i| start + Duration::days(i)).collect()
            }
            Freq::Monthly => {
                let m = i64::from(first.year()) * 12 + i64::from(first.month0()) + step;
                let start = NaiveDate::from_ymd_opt(
                    i32::try_from(m.div_euclid(12)).ok()?,
                    m.rem_euclid(12) as u32 + 1,
                    1,
                )?;
                start
                    .iter_days()
                    .take_while(|d| d.month() == start.month())
                    .collect()
            }
            Freq::Yearly => {
                let start =
                    NaiveDate::from_ymd_opt(first.year() + i32::try_from(step).ok()?, 1, 1)?;
                start
                    .iter_days()
                    .take_while(|d| d.year() == start.year())
                    .collect()
            }
        };
        (days.first()?.year() <= LAST_YEAR).then_some(days)
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        let dim = days_in_month(d);
        let diy = if d.leap_year() { 366 } else { 365 };
        let nth = |n: i32, pos: u32, len: u32| match n {
            n if n > 0 => (pos - 1) / 7 + 1 == n as u32,
            n => (len - pos) / 7 + 1 == n.unsigned_abs(),
        };
        let within_month = self.freq == Freq::Monthly || !self.by_month.is_empty();
        (self.by_month.is_empty() || self.by_month.contains(&d.month()))
            && (self.by_month_day.is_empty()
                || self
                    .by_month_day
                    .iter()
                    .any(|&md| signed_is(md, d.day(), dim)))
            && (self.by_year_day.is_empty()
                || self
                    .by_year_day
                    .iter()
                    .any(|&yd| signed_is(yd, d.ordinal(), diy)))
            && (self.by_day.is_empty()
                || self.by_day.iter().any(|&(n, wd)| {
                    d.weekday() == wd
                        && match n {
                            None => true,
                            Some(n) if within_month => nth(n, d.day(), dim),
                            Some(n) => nth(n, d.ordinal(), diy),
                        }
                }))
    }

    fn period(&self, first: NaiveDate, k: i64) -> Option<Vec<NaiveDateTime>> {
        let mut out = Vec::new();
        for d in self.period_days(first, k)? {
            if !self.day_matches(d) {
                continue;
            }
            for &h in &self.by_hour {
                for &m in &self.by_minute {
                    for &s in &self.by_second {
                        out.extend(NaiveTime::from_hms_opt(h, m, s).map(|t| d.and_time(t)));
                    }
                }
            }
        }
        if self.by_set_pos.is_empty() {
            return Some(out);
        }
        let len = out.len() as i64;
        let mut picked: Vec<NaiveDateTime> = self
            .by_set_pos
            .iter()
            .filter_map(|&p| {
                let i = if p > 0 {
                    i64::from(p) - 1
                } else {
                    len + i64::from(p)
                };
                usize::try_from(i).ok().and_then(|i| out.get(i).copied())
            })
            .collect();
        picked.sort_unstable();
        picked.dedup();
        Some(picked)
    }
}

/// [`Rule::starts`].
pub struct Starts {
    rule: Rule,
    first: NaiveDateTime,
    period: i64,
    /// The current period's starts still to give, latest first.
    pending: Vec<NaiveDateTime>,
    emitted: u32,
    first_done: bool,
    exhausted: bool,
}

impl Iterator for Starts {
    type Item = NaiveDateTime;

    fn next(&mut self) -> Option<NaiveDateTime> {
        if self.rule.count.is_some_and(|c| self.emitted >= c) {
            return None;
        }
        if !self.first_done {
            self.first_done = true;
            self.emitted += 1;
            return Some(self.first);
        }
        loop {
            if let Some(t) = self.pending.pop() {
                self.emitted += 1;
                return Some(t);
            }
            if self.exhausted {
                return None;
            }
            match self.rule.period(self.first.date(), self.period) {
                Some(mut starts) => {
                    starts.retain(|t| *t > self.first);
                    starts.reverse();
                    self.pending = starts;
                    self.period += 1;
                }
                None => self.exhausted = true,
            }
        }
    }
}

fn signed_is(want: i32, pos: u32, len: u32) -> bool {
    if want > 0 {
        pos == want as u32
    } else {
        len + 1 >= want.unsigned_abs() && pos == len + 1 - want.unsigned_abs()
    }
}

fn days_in_month(d: NaiveDate) -> u32 {
    let next = if d.month() == 12 {
        NaiveDate::from_ymd_opt(d.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1)
    };
    next.map_or(31, |n| n.pred_opt().map_or(31, |l| l.day()))
}

fn numbers<T: std::str::FromStr + Copy + Into<i64>>(
    v: &str,
    min: i64,
    max: i64,
    negative: bool,
) -> Option<Vec<T>> {
    v.split(',')
        .map(|n| {
            let parsed: T = n.trim().trim_start_matches('+').parse().ok()?;
            let x: i64 = parsed.into();
            let ok = (min..=max).contains(&x) || (negative && (-max..=-min).contains(&x));
            ok.then_some(parsed)
        })
        .collect()
}

fn until(v: &str) -> Option<Until> {
    if v.len() == 8 {
        return NaiveDate::parse_from_str(v, "%Y%m%d").ok().map(Until::Date);
    }
    match v.strip_suffix('Z').or_else(|| v.strip_suffix('z')) {
        Some(body) => NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M%S")
            .ok()
            .map(Until::Utc),
        None => NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S")
            .ok()
            .map(Until::Local),
    }
}

fn by_day(d: &str) -> Option<(Option<i32>, Weekday)> {
    let split = d.len().checked_sub(2)?;
    let (ord, code) = d.split_at(split);
    let wd = weekday(code)?;
    if ord.is_empty() {
        return Some((None, wd));
    }
    let n: i32 = ord.trim_start_matches('+').parse().ok()?;
    ((1..=53).contains(&n.abs())).then_some((Some(n), wd))
}

fn weekday(code: &str) -> Option<Weekday> {
    Some(match code.to_ascii_uppercase().as_str() {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").unwrap()
    }

    fn first_n(rule: &str, start: &str, n: usize) -> Vec<String> {
        Rule::parse(rule)
            .unwrap()
            .starts(at(start))
            .take(n)
            .map(|t| t.format("%Y%m%dT%H%M").to_string())
            .collect()
    }

    fn dates(rule: &str, start: &str, n: usize) -> Vec<String> {
        first_n(rule, start, n)
            .into_iter()
            .map(|s| s[..8].to_string())
            .collect()
    }

    // The examples below are RFC 5545 §3.8.5.3's, with its answers.

    #[test]
    fn daily_for_ten_occurrences() {
        let got = dates("FREQ=DAILY;COUNT=10", "19970902T090000", 20);
        assert_eq!(got.len(), 10);
        assert_eq!(got[9], "19970911");
    }

    #[test]
    fn every_other_day_and_every_ten_days_five_times() {
        assert_eq!(
            dates("FREQ=DAILY;INTERVAL=2", "19970902T090000", 3),
            ["19970902", "19970904", "19970906"]
        );
        assert_eq!(
            dates("FREQ=DAILY;INTERVAL=10;COUNT=5", "19970902T090000", 9),
            ["19970902", "19970912", "19970922", "19971002", "19971012"]
        );
    }

    #[test]
    fn weekly_on_tuesday_and_thursday_for_five_weeks() {
        let got = dates(
            "FREQ=WEEKLY;COUNT=10;WKST=SU;BYDAY=TU,TH",
            "19970902T090000",
            20,
        );
        assert_eq!(
            got,
            [
                "19970902", "19970904", "19970909", "19970911", "19970916", "19970918", "19970923",
                "19970925", "19970930", "19971002"
            ]
        );
    }

    #[test]
    fn every_other_week_on_monday_wednesday_and_friday() {
        let got = dates(
            "FREQ=WEEKLY;INTERVAL=2;WKST=SU;BYDAY=MO,WE,FR",
            "19970901T090000",
            7,
        );
        assert_eq!(
            got,
            ["19970901", "19970903", "19970905", "19970915", "19970917", "19970919", "19970929"]
        );
    }

    /// WKST changes the answer when the interval is above one: the RFC's
    /// own pair of examples.
    #[test]
    fn the_week_start_decides_which_week_a_day_is_in() {
        assert_eq!(
            dates(
                "FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=MO",
                "19970805T090000",
                9
            ),
            ["19970805", "19970810", "19970819", "19970824"]
        );
        assert_eq!(
            dates(
                "FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=SU",
                "19970805T090000",
                9
            ),
            ["19970805", "19970817", "19970819", "19970831"]
        );
    }

    #[test]
    fn monthly_on_the_first_friday_and_on_the_last_and_second_to_last_day() {
        assert_eq!(
            dates("FREQ=MONTHLY;COUNT=10;BYDAY=1FR", "19970905T090000", 3),
            ["19970905", "19971003", "19971107"]
        );
        assert_eq!(
            dates("FREQ=MONTHLY;BYMONTHDAY=-3", "19970928T090000", 4),
            ["19970928", "19971029", "19971128", "19971229"]
        );
        assert_eq!(
            dates("FREQ=MONTHLY;BYDAY=-2MO;COUNT=6", "19970922T090000", 3),
            ["19970922", "19971020", "19971117"]
        );
    }

    #[test]
    fn a_day_a_month_lacks_is_skipped_not_moved() {
        assert_eq!(
            dates("FREQ=MONTHLY;BYMONTHDAY=31", "20260131T090000", 4),
            ["20260131", "20260331", "20260531", "20260731"]
        );
        assert_eq!(
            dates("FREQ=YEARLY", "20240229T090000", 3),
            ["20240229", "20280229", "20320229"]
        );
    }

    #[test]
    fn yearly_by_month_and_by_weekday() {
        assert_eq!(
            dates("FREQ=YEARLY;COUNT=10;BYMONTH=6,7", "19970610T090000", 4),
            ["19970610", "19970710", "19980610", "19980710"]
        );
        // US Thanksgiving, and the RFC's "every Thursday in March".
        assert_eq!(
            dates("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH", "20261126T090000", 3),
            ["20261126", "20271125", "20281123"]
        );
        assert_eq!(
            dates("FREQ=YEARLY;BYDAY=TH;BYMONTH=3", "19970313T090000", 4),
            ["19970313", "19970320", "19970327", "19980305"]
        );
        // The 20th Monday of the year.
        assert_eq!(
            dates("FREQ=YEARLY;BYDAY=20MO", "19970519T090000", 3),
            ["19970519", "19980518", "19990517"]
        );
    }

    #[test]
    fn the_last_weekday_of_the_month_by_set_position() {
        assert_eq!(
            dates(
                "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
                "19970930T090000",
                3
            ),
            ["19970930", "19971031", "19971128"]
        );
        assert_eq!(
            dates(
                "FREQ=MONTHLY;COUNT=3;BYDAY=TU,WE,TH;BYSETPOS=3",
                "19970904T090000",
                9
            ),
            ["19970904", "19971007", "19971106"]
        );
    }

    #[test]
    fn hours_within_a_day() {
        assert_eq!(
            first_n("FREQ=DAILY;BYHOUR=9,17;COUNT=4", "20260105T090000", 9),
            [
                "20260105T0900",
                "20260105T1700",
                "20260106T0900",
                "20260106T1700"
            ]
        );
    }

    /// RFC 5545 counts DTSTART as the first occurrence even when the rule
    /// would not give it; Google shows it.
    #[test]
    fn a_first_start_off_the_rule_still_counts() {
        assert_eq!(
            dates("FREQ=WEEKLY;BYDAY=MO;COUNT=3", "20260107T090000", 9),
            ["20260107", "20260112", "20260119"]
        );
    }

    #[test]
    fn a_rule_that_matches_nothing_ends() {
        let n = Rule::parse("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30")
            .unwrap()
            .starts(at("20260101T090000"))
            .count();
        assert_eq!(n, 1, "only the first start");
    }

    #[test]
    fn what_it_cannot_follow_is_refused_by_name() {
        assert_eq!(
            Rule::parse("FREQ=YEARLY;BYWEEKNO=20").unwrap_err(),
            "cannot follow `BYWEEKNO=20`"
        );
        assert!(Rule::parse("FREQ=HOURLY").is_err());
        assert!(Rule::parse("BYDAY=MO").is_err());
        assert!(Rule::parse("FREQ=DAILY;COUNT=2;UNTIL=20260101").is_err());
        assert!(Rule::parse("FREQ=MONTHLY;BYMONTHDAY=0").is_err());
    }

    #[test]
    fn until_keeps_the_form_it_was_written_in() {
        let u = |s: &str| Rule::parse(&format!("FREQ=DAILY;UNTIL={s}")).unwrap().until;
        assert_eq!(
            u("20261231"),
            Some(Until::Date(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()))
        );
        assert_eq!(
            u("20261231T170000Z"),
            Some(Until::Utc(at("20261231T170000")))
        );
        assert_eq!(
            u("20261231T090000"),
            Some(Until::Local(at("20261231T090000")))
        );
    }
}
