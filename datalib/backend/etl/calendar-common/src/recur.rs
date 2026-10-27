//! An RFC 5545 `RRULE` run forward on a wall clock, by
//! [`calcard`](https://github.com/stalwartlabs/calcard): the one file
//! that names it. The rule is run on floating time, so a weekly 9am
//! stays 9am; which zone that clock is in, `UNTIL`, and the dates a
//! series no longer has are [`crate::occurrences`]'s.

use calcard::datecalc::rrule::RRule;
use calcard::icalendar::{ICalendar, ICalendarProperty, ICalendarValue};
use chrono::{NaiveDate, NaiveDateTime};

/// More starts than any window asks for; a rule that would give more
/// before `stop` is refused rather than cut short.
const MOST: usize = 1_000_000;

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

/// An `RRULE` value (`FREQ=WEEKLY;BYDAY=MO`), with or without its
/// `RRULE:` name, split into the rule calcard runs and its `UNTIL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    without_until: String,
    pub until: Option<Until>,
}

impl Rule {
    /// `Err` says what in the rule cannot be read.
    pub fn parse(rule: &str) -> Result<Rule, String> {
        let body = rule.trim();
        let body = body.strip_prefix("RRULE:").unwrap_or(body);
        let mut until = None;
        let mut kept = Vec::new();
        for part in body.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            match part.split_once('=') {
                Some((k, v)) if k.trim().eq_ignore_ascii_case("UNTIL") => {
                    until = Some(
                        parse_until(v.trim()).ok_or_else(|| format!("cannot follow `{part}`"))?,
                    );
                }
                _ => kept.push(part),
            }
        }
        Ok(Rule {
            without_until: kept.join(";"),
            until,
        })
    }

    /// The wall-clock starts this rule gives a series that first starts
    /// at `first`, in order and before `stop`, `first` included (RFC
    /// 5545 counts it as the first occurrence). `all_day` runs it on
    /// dates. `COUNT` is applied here; `UNTIL` is the caller's.
    pub fn starts(
        &self,
        first: NaiveDateTime,
        all_day: bool,
        stop: NaiveDateTime,
    ) -> Result<Vec<NaiveDateTime>, String> {
        let spell = |t: NaiveDateTime| match all_day {
            true => t.format("%Y%m%d").to_string(),
            false => t.format("%Y%m%dT%H%M%S").to_string(),
        };
        let value = if all_day { ";VALUE=DATE" } else { "" };
        // `stop` bounds the run as an `UNTIL`, except beside a `COUNT`,
        // which bounds it already and may not share a rule with one.
        let counted = self
            .without_until
            .split(';')
            .any(|p| p.trim().to_ascii_uppercase().starts_with("COUNT="));
        let bound = match counted {
            true => String::new(),
            false => format!(";UNTIL={}", spell(stop)),
        };
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//datalib//recur//EN\r\n\
             BEGIN:VEVENT\r\nUID:recur\r\nDTSTART{value}:{}\r\nDURATION:PT0S\r\n\
             RRULE:{}{bound}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            spell(first),
            self.without_until,
        );
        let refused = || format!("cannot follow `{}`", self.without_until);
        let cal = ICalendar::parse(&ics).map_err(|_| refused())?;
        // A rule calcard cannot read leaves the event with no rule at all,
        // which it would list once, as a one-off.
        let read = cal.components.iter().flat_map(|c| &c.entries).any(|e| {
            match (&e.name, e.values.first()) {
                (ICalendarProperty::Rrule, Some(ICalendarValue::RecurrenceRule(r))) => {
                    RRule::from_floating_ical(r).is_some()
                }
                _ => false,
            }
        });
        if !read {
            return Err(refused());
        }
        let expanded = cal.expand_dates(chrono::Utc, MOST);
        if let Some(e) = expanded.errors.first() {
            return Err(format!("{}: {:?}", refused(), e.error));
        }
        if expanded.events.len() >= MOST {
            return Err(format!("{} gives over {MOST} starts", refused()));
        }
        let mut starts: Vec<NaiveDateTime> = expanded
            .events
            .iter()
            .map(|e| e.start.naive_local())
            .filter(|t| *t < stop)
            .collect();
        starts.push(first);
        starts.sort_unstable();
        starts.dedup();
        Ok(starts)
    }
}

fn parse_until(v: &str) -> Option<Until> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").unwrap()
    }

    fn first_n(rule: &str, start: &str, n: usize) -> Vec<String> {
        let first = at(start);
        let stop = first + chrono::Duration::days(366 * 10);
        Rule::parse(rule)
            .unwrap()
            .starts(first, false, stop)
            .unwrap()
            .into_iter()
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
    fn monthly_on_the_first_friday_and_on_the_third_to_last_day() {
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
    fn yearly_by_month_by_weekday_and_by_week_number() {
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
        // The 20th Monday of the year, and Monday of week 20.
        assert_eq!(
            dates("FREQ=YEARLY;BYDAY=20MO", "19970519T090000", 3),
            ["19970519", "19980518", "19990517"]
        );
        assert_eq!(
            dates("FREQ=YEARLY;BYWEEKNO=20;BYDAY=MO", "19970512T090000", 3),
            ["19970512", "19980511", "19990517"]
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

    #[test]
    fn all_day_runs_on_dates() {
        let first = at("20261224T000000");
        let stop = at("20300101T000000");
        let got = Rule::parse("FREQ=YEARLY")
            .unwrap()
            .starts(first, true, stop)
            .unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[3], at("20291224T000000"));
    }

    /// RFC 5545 counts DTSTART as the first occurrence even when the rule
    /// would not give it; Google shows it.
    #[test]
    fn a_first_start_off_the_rule_still_counts() {
        assert_eq!(
            dates("FREQ=WEEKLY;BYDAY=MO;COUNT=3", "20260107T090000", 9)[0],
            "20260107"
        );
    }

    #[test]
    fn a_rule_that_matches_nothing_gives_only_its_first_start() {
        let first = at("20260101T090000");
        let got = Rule::parse("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30")
            .unwrap()
            .starts(first, false, at("20360101T000000"))
            .unwrap();
        assert_eq!(got, [first]);
    }

    #[test]
    fn what_it_cannot_follow_is_refused_by_name() {
        let starts = |rule: &str| {
            Rule::parse(rule)?.starts(at("20260101T090000"), false, at("20270101T000000"))
        };
        assert!(starts("FREQ=FORTNIGHTLY").is_err());
        assert!(starts("BYDAY=MO").is_err());
        assert!(starts("FREQ=DAILY;UNTIL=soon").is_err());
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
