//! A series' occurrences in a window of time: its rules run forward in
//! the zone its first start is written in, with `UNTIL` compared the
//! way it was written, the dates it no longer has taken out, and its
//! extra dates added. What replaces a removed date (a moved occurrence)
//! is the provider's to add back: only it knows the occurrence's id.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use datalib_time::NamedZone;

use crate::recur::{Rule, Until};
use crate::when::{EventTime, Zone};

/// `[from, until)`: an occurrence is in it when any of it is, as
/// Google's `timeMin` and `timeMax` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

/// A recurring event as its first occurrence describes it.
#[derive(Debug, Clone, Copy)]
pub struct Series<'a> {
    pub start: &'a EventTime,
    pub end: Option<&'a EventTime>,
    /// `RRULE` values.
    pub rules: &'a [String],
    /// `RDATE`s: dates no rule gives.
    pub rdates: &'a [EventTime],
    /// The dates the series' own occurrence no longer stands on:
    /// `EXDATE`s, and the original start of every occurrence that was
    /// moved, edited or cancelled on its own.
    pub removed: &'a [EventTime],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub start: EventTime,
    pub end: Option<EventTime>,
}

/// Why a series' occurrences cannot be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unexpandable {
    /// A rule part this build does not follow, named.
    Rule(String),
    /// A `TZID` the zone database does not know.
    Zone(String),
}

impl std::fmt::Display for Unexpandable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unexpandable::Rule(why) => write!(f, "repeat rule: {why}"),
            Unexpandable::Zone(z) => write!(f, "unknown time zone {z:?}"),
        }
    }
}

/// Every occurrence of `series` in `window`, by start. A floating time,
/// and an all-day date, is read in `calendar_zone`.
pub fn expand(
    series: &Series<'_>,
    window: &Window,
    calendar_zone: Option<&str>,
) -> Result<Vec<Occurrence>, Unexpandable> {
    let rules: Vec<Rule> = series
        .rules
        .iter()
        .map(|r| Rule::parse(r).map_err(Unexpandable::Rule))
        .collect::<Result<_, _>>()?;
    let start_zone = Clock::of(series.start, calendar_zone)?;
    let length = Length::between(series.start, series.end, calendar_zone)?;
    let first = start_zone.local_of(series.start);
    let removed = Removed::new(series.removed, calendar_zone);

    let mut by_key: BTreeMap<String, Occurrence> = BTreeMap::new();
    let mut keep = |start: EventTime| -> Result<(), Unexpandable> {
        if removed.has(&start) {
            return Ok(());
        }
        let end = length.after(&start, calendar_zone)?;
        let (s, e) = span(&start, end.as_ref(), calendar_zone)?;
        if s < window.until && (e > window.from || (e == s && s >= window.from)) {
            by_key.insert(sort_key(s, &start), Occurrence { start, end });
        }
        Ok(())
    };
    for rule in &rules {
        for local in rule.starts(first) {
            if !start_zone.within_until(local, rule.until.as_ref()) {
                break;
            }
            let start = start_zone.time_at(local);
            if instant_of(&start, calendar_zone)? >= window.until {
                break;
            }
            keep(start)?;
        }
    }
    if rules.is_empty() {
        keep(series.start.clone())?;
    }
    for rdate in series.rdates {
        keep(rdate.clone())?;
    }
    Ok(by_key.into_values().collect())
}

/// When `start` to `end` begins and ends. An all-day date begins at
/// midnight where the calendar is, which is when Google's window takes
/// it to; with no zone, at midnight UTC.
pub fn span(
    start: &EventTime,
    end: Option<&EventTime>,
    calendar_zone: Option<&str>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), Unexpandable> {
    let s = instant_of(start, calendar_zone)?;
    let e = match end {
        Some(end) => instant_of(end, calendar_zone)?.max(s),
        // RFC 5545: an all-day event with no end lasts the day.
        None if start.is_all_day() => instant_of(&next_day(start), calendar_zone)?,
        None => s,
    };
    Ok((s, e))
}

/// The instant `t` begins at, all-day dates where the calendar is.
pub fn instant_of(
    t: &EventTime,
    calendar_zone: Option<&str>,
) -> Result<DateTime<Utc>, Unexpandable> {
    let t = match t {
        EventTime::Date(d) => EventTime::At {
            local: d.and_hms_opt(0, 0, 0).expect("midnight exists"),
            zone: Zone::Floating,
        },
        other => other.clone(),
    };
    t.instant(calendar_zone)
        .map(|i| i.at.with_timezone(&Utc))
        .map_err(|e| match e {
            crate::when::Unresolved::UnknownZone(z) => Unexpandable::Zone(z),
            crate::when::Unresolved::BadOffset(o) => Unexpandable::Zone(format!("UTC{o:+}s")),
        })
}

fn next_day(t: &EventTime) -> EventTime {
    match t {
        EventTime::Date(d) => EventTime::Date(*d + Duration::days(1)),
        other => other.clone(),
    }
}

/// Orders occurrences by when they begin; the key breaks a tie between
/// two that begin together, so neither is lost.
fn sort_key(at: DateTime<Utc>, start: &EventTime) -> String {
    format!("{}|{}", at.to_rfc3339(), start.key(None))
}

/// The clock a series runs on: the zone its first start is written in.
enum Clock {
    Date,
    Named(NamedZone, String),
    /// Google's times: a named zone, written with the offset there.
    GoogleNamed(NamedZone, String),
    Fixed(i32),
    Utc,
    /// Floating, run on the calendar's zone where it has one.
    Floating(Option<NamedZone>),
}

impl Clock {
    fn of(t: &EventTime, calendar_zone: Option<&str>) -> Result<Clock, Unexpandable> {
        let tz = |n: &str| NamedZone::get(n).ok_or_else(|| Unexpandable::Zone(n.to_string()));
        Ok(match t {
            EventTime::Date(_) => Clock::Date,
            EventTime::At { zone, .. } => match zone {
                Zone::Named(n) => Clock::Named(tz(n)?, n.clone()),
                Zone::Fixed {
                    name: Some(n),
                    offset_seconds,
                } => match tz(n) {
                    Ok(z) => Clock::GoogleNamed(z, n.clone()),
                    Err(_) => Clock::Fixed(*offset_seconds),
                },
                Zone::Fixed {
                    offset_seconds,
                    name: None,
                } => Clock::Fixed(*offset_seconds),
                Zone::Utc => Clock::Utc,
                Zone::Floating => Clock::Floating(calendar_zone.and_then(NamedZone::get)),
            },
        })
    }

    fn local_of(&self, t: &EventTime) -> NaiveDateTime {
        match t {
            EventTime::Date(d) => d.and_hms_opt(0, 0, 0).expect("midnight exists"),
            EventTime::At { local, .. } => *local,
        }
    }

    /// The time this clock reads `local` as, written the way the
    /// series' first start is.
    fn time_at(&self, local: NaiveDateTime) -> EventTime {
        let zone = match self {
            Clock::Date => return EventTime::Date(local.date()),
            Clock::Named(_, n) => Zone::Named(n.clone()),
            Clock::GoogleNamed(tz, n) => {
                let at = resolve(tz, local);
                return EventTime::At {
                    local: at.naive_local(),
                    zone: Zone::Fixed {
                        offset_seconds: at.offset().local_minus_utc(),
                        name: Some(n.clone()),
                    },
                };
            }
            Clock::Fixed(o) => Zone::Fixed {
                offset_seconds: *o,
                name: None,
            },
            Clock::Utc => Zone::Utc,
            Clock::Floating(_) => Zone::Floating,
        };
        EventTime::At { local, zone }
    }

    /// Whether `local` is on or before `UNTIL`. A UTC `UNTIL` is an
    /// instant, so `local` is read in the series' zone first; a date
    /// takes in the whole of its day there.
    fn within_until(&self, local: NaiveDateTime, until: Option<&Until>) -> bool {
        match until {
            None => true,
            Some(Until::Date(d)) => local.date() <= *d,
            Some(Until::Local(u)) => local <= *u,
            Some(Until::Utc(u)) => self.utc_of(local) <= u.and_utc(),
        }
    }

    fn utc_of(&self, local: NaiveDateTime) -> DateTime<Utc> {
        match self {
            Clock::Named(tz, _) | Clock::GoogleNamed(tz, _) | Clock::Floating(Some(tz)) => {
                resolve(tz, local).with_timezone(&Utc)
            }
            Clock::Fixed(o) => FixedOffset::east_opt(*o)
                .and_then(|z| z.from_local_datetime(&local).single())
                .map_or_else(|| local.and_utc(), |t| t.with_timezone(&Utc)),
            Clock::Date | Clock::Utc | Clock::Floating(None) => local.and_utc(),
        }
    }
}

/// A local time in `tz`; one in the spring-forward gap moves past it,
/// as `EventTime::instant` reads it (RFC 5545 §3.3.5).
fn resolve(tz: &NamedZone, local: NaiveDateTime) -> DateTime<FixedOffset> {
    tz.from_local(&local)
        .earliest()
        .or_else(|| tz.from_local(&(local + Duration::hours(1))).earliest())
        .unwrap_or_else(|| local.and_utc().fixed_offset())
}

/// How long each occurrence lasts. On the series' own clock when the
/// end is written in the same zone as the start, so a meeting that
/// crosses a clock change keeps its wall-clock end; otherwise (Google
/// lets a flight end in another zone) the exact span, ended in the
/// end's own zone.
enum Length {
    None,
    Days(i64),
    Wall(Duration),
    Exact(Duration, Clock),
}

impl Length {
    fn between(
        start: &EventTime,
        end: Option<&EventTime>,
        calendar_zone: Option<&str>,
    ) -> Result<Length, Unexpandable> {
        let Some(end) = end else {
            return Ok(Length::None);
        };
        Ok(match (start, end) {
            (EventTime::Date(s), EventTime::Date(e)) => Length::Days((*e - *s).num_days()),
            (EventTime::At { local: s, .. }, EventTime::At { local: e, .. })
                if start.zone_name() == end.zone_name() && same_kind(start, end) =>
            {
                Length::Wall(*e - *s)
            }
            _ => Length::Exact(
                instant_of(end, calendar_zone)? - instant_of(start, calendar_zone)?,
                Clock::of(end, calendar_zone)?,
            ),
        })
    }

    fn after(
        &self,
        start: &EventTime,
        calendar_zone: Option<&str>,
    ) -> Result<Option<EventTime>, Unexpandable> {
        Ok(match (self, start) {
            (Length::None, _) => None,
            (Length::Days(n), EventTime::Date(d)) => Some(EventTime::Date(*d + Duration::days(*n))),
            (Length::Wall(d), EventTime::At { local, zone }) => {
                let local = *local + *d;
                Some(match zone {
                    Zone::Fixed {
                        name: Some(n),
                        offset_seconds,
                    } => match NamedZone::get(n) {
                        Some(tz) => Clock::GoogleNamed(tz, n.clone()).time_at(local),
                        None => Clock::Fixed(*offset_seconds).time_at(local),
                    },
                    zone => EventTime::At {
                        local,
                        zone: zone.clone(),
                    },
                })
            }
            (Length::Exact(d, clock), _) => {
                let at = instant_of(start, calendar_zone)? + *d;
                Some(clock.time_at_instant(at))
            }
            // A start whose shape differs from the first one's is an
            // RDATE written another way; it lasts no time.
            _ => None,
        })
    }
}

fn same_kind(a: &EventTime, b: &EventTime) -> bool {
    let kind = |t: &EventTime| match t {
        EventTime::Date(_) => 0,
        EventTime::At { zone, .. } => match zone {
            Zone::Utc => 1,
            Zone::Named(_) => 2,
            Zone::Fixed { .. } => 3,
            Zone::Floating => 4,
        },
    };
    kind(a) == kind(b)
}

impl Clock {
    fn time_at_instant(&self, at: DateTime<Utc>) -> EventTime {
        self.time_at(self.local_at(at))
    }

    /// What this clock reads at `at`.
    fn local_at(&self, at: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Clock::Named(tz, _) | Clock::GoogleNamed(tz, _) | Clock::Floating(Some(tz)) => {
                tz.at(&at).naive_local()
            }
            Clock::Fixed(o) => FixedOffset::east_opt(*o)
                .map_or_else(|| at.naive_utc(), |z| at.with_timezone(&z).naive_local()),
            Clock::Date | Clock::Utc | Clock::Floating(None) => at.naive_utc(),
        }
    }
}

/// The dates a series no longer stands on, matched the way a
/// `RECURRENCE-ID` is: by the instant, however it was written. A bare
/// date against a timed series takes out that day's occurrence.
struct Removed<'a> {
    keys: Vec<String>,
    days: Vec<NaiveDate>,
    calendar_zone: Option<&'a str>,
}

impl<'a> Removed<'a> {
    fn new(removed: &[EventTime], calendar_zone: Option<&'a str>) -> Removed<'a> {
        let mut keys: Vec<String> = removed.iter().map(|t| t.key(calendar_zone)).collect();
        keys.sort_unstable();
        keys.dedup();
        let days = removed
            .iter()
            .filter_map(|t| match t {
                EventTime::Date(d) => Some(*d),
                EventTime::At { .. } => None,
            })
            .collect();
        Removed {
            keys,
            days,
            calendar_zone,
        }
    }

    fn has(&self, start: &EventTime) -> bool {
        self.keys
            .binary_search(&start.key(self.calendar_zone))
            .is_ok()
            || match start {
                EventTime::At { local, .. } => self.days.contains(&local.date()),
                EventTime::Date(_) => false,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LA: &str = "America/Los_Angeles";

    fn ical(v: &str, tz: Option<&str>) -> EventTime {
        EventTime::from_ical(v, tz).unwrap()
    }

    fn window(from: &str, until: &str) -> Window {
        let p = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        Window {
            from: p(from),
            until: p(until),
        }
    }

    fn starts(occ: &[Occurrence], zone: Option<&str>) -> Vec<String> {
        occ.iter()
            .map(|o| {
                let at = instant_of(&o.start, zone).unwrap();
                NamedZone::get(zone.unwrap_or("UTC"))
                    .unwrap()
                    .at(&at)
                    .to_rfc3339()
            })
            .collect()
    }

    #[test]
    fn a_weekly_nine_am_stays_nine_am_across_daylight_saving() {
        let start = ical("20261026T090000", Some(LA));
        let end = ical("20261026T100000", Some(LA));
        let rules = ["FREQ=WEEKLY;BYDAY=MO".to_string()];
        let s = Series {
            start: &start,
            end: Some(&end),
            rules: &rules,
            rdates: &[],
            removed: &[],
        };
        let occ = expand(
            &s,
            &window("2026-10-26T00:00:00Z", "2026-11-10T00:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(
            starts(&occ, Some(LA)),
            [
                "2026-10-26T09:00:00-07:00",
                "2026-11-02T09:00:00-08:00",
                "2026-11-09T09:00:00-08:00"
            ]
        );
        assert_eq!(occ[1].end, Some(ical("20261102T100000", Some(LA))));
    }

    /// `UNTIL` is an instant in UTC: 17:00Z is 09:00 in Los Angeles in
    /// winter, so the 31 Dec occurrence at 09:00 is the last one, but a
    /// rule ending at 16:59Z stops the day before.
    #[test]
    fn until_in_utc_is_compared_as_an_instant_in_the_series_zone() {
        let start = ical("20261201T090000", Some(LA));
        let mk = |rule: &str| {
            let rules = [rule.to_string()];
            let s = Series {
                start: &start,
                end: None,
                rules: &rules,
                rdates: &[],
                removed: &[],
            };
            let occ = expand(
                &s,
                &window("2026-12-01T00:00:00Z", "2027-02-01T00:00:00Z"),
                None,
            )
            .unwrap();
            occ.last().unwrap().start.display_date()
        };
        assert_eq!(mk("FREQ=DAILY;UNTIL=20261231T170000Z"), "Thu 31 Dec 2026");
        assert_eq!(mk("FREQ=DAILY;UNTIL=20261231T165900Z"), "Wed 30 Dec 2026");
        // A bare date takes in the whole of its day.
        assert_eq!(mk("FREQ=DAILY;UNTIL=20261231"), "Thu 31 Dec 2026");
    }

    /// `EXDATE;TZID=…` and an `EXDATE` in UTC name the same occurrence;
    /// a Google cancelled row's original start, written with an offset,
    /// does too.
    #[test]
    fn a_removed_date_matches_however_it_was_written() {
        let start = ical("20261005T090000", Some(LA));
        let rules = ["FREQ=WEEKLY;BYDAY=MO;COUNT=5".to_string()];
        let removed = [
            ical("20261012T090000", Some(LA)),
            ical("20261019T160000Z", None),
            EventTime::from_google(None, Some("2026-11-02T12:00:00-05:00"), Some(LA)).unwrap(),
        ];
        let s = Series {
            start: &start,
            end: None,
            rules: &rules,
            rdates: &[],
            removed: &removed,
        };
        let occ = expand(
            &s,
            &window("2026-10-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(
            starts(&occ, Some(LA)),
            ["2026-10-05T09:00:00-07:00", "2026-10-26T09:00:00-07:00"]
        );
    }

    #[test]
    fn an_all_day_series_runs_on_dates_and_its_window_is_the_calendars_day() {
        let start = EventTime::Date(NaiveDate::from_ymd_opt(2026, 12, 24).unwrap());
        let end = EventTime::Date(NaiveDate::from_ymd_opt(2026, 12, 26).unwrap());
        let rules = ["FREQ=YEARLY".to_string()];
        let removed = [EventTime::Date(
            NaiveDate::from_ymd_opt(2027, 12, 24).unwrap(),
        )];
        let s = Series {
            start: &start,
            end: Some(&end),
            rules: &rules,
            rdates: &[],
            removed: &removed,
        };
        let occ = expand(
            &s,
            &window("2026-01-01T00:00:00Z", "2029-01-01T00:00:00Z"),
            Some(LA),
        )
        .unwrap();
        let days: Vec<String> = occ.iter().map(|o| o.start.display_date()).collect();
        assert_eq!(days, ["Thu 24 Dec 2026", "Sun 24 Dec 2028"]);
        assert_eq!(
            occ[1].end,
            Some(EventTime::Date(
                NaiveDate::from_ymd_opt(2028, 12, 26).unwrap()
            ))
        );
        // The 24th in Los Angeles begins at 08:00 UTC.
        let (s, e) = span(&occ[0].start, occ[0].end.as_ref(), Some(LA)).unwrap();
        assert_eq!(s.to_rfc3339(), "2026-12-24T08:00:00+00:00");
        assert_eq!(e.to_rfc3339(), "2026-12-26T08:00:00+00:00");
    }

    /// Google writes a series' start with the calendar's offset; each
    /// occurrence carries the offset its own date has there.
    #[test]
    fn a_google_start_keeps_its_zone_and_moves_its_offset() {
        let start =
            EventTime::from_google(None, Some("2026-10-30T09:00:00-07:00"), Some(LA)).unwrap();
        let end =
            EventTime::from_google(None, Some("2026-10-30T09:30:00-07:00"), Some(LA)).unwrap();
        let rules = ["FREQ=WEEKLY;BYDAY=FR;COUNT=2".to_string()];
        let s = Series {
            start: &start,
            end: Some(&end),
            rules: &rules,
            rdates: &[],
            removed: &[],
        };
        let occ = expand(
            &s,
            &window("2026-10-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(
            occ[1].start,
            EventTime::from_google(None, Some("2026-11-06T09:00:00-08:00"), Some(LA)).unwrap()
        );
        assert_eq!(
            occ[1].end,
            EventTime::from_google(None, Some("2026-11-06T09:30:00-08:00"), Some(LA))
        );
    }

    /// An occurrence that began before the window and is still going is
    /// in it, as it is in Google's `timeMin`.
    #[test]
    fn the_window_takes_what_overlaps_it() {
        let start = ical("20261001T220000Z", None);
        let end = ical("20261002T020000Z", None);
        let rules = ["FREQ=DAILY".to_string()];
        let s = Series {
            start: &start,
            end: Some(&end),
            rules: &rules,
            rdates: &[ical("20261005T120000Z", None)],
            removed: &[],
        };
        let occ = expand(
            &s,
            &window("2026-10-04T00:00:00Z", "2026-10-06T00:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(
            starts(&occ, None),
            [
                "2026-10-03T22:00:00+00:00",
                "2026-10-04T22:00:00+00:00",
                "2026-10-05T12:00:00+00:00",
                "2026-10-05T22:00:00+00:00"
            ]
        );
    }

    #[test]
    fn a_zone_or_rule_it_cannot_read_is_said_not_guessed() {
        let start = ical("20261001T090000", Some("Pacific Standard Time"));
        let rules = ["FREQ=DAILY".to_string()];
        let s = Series {
            start: &start,
            end: None,
            rules: &rules,
            rdates: &[],
            removed: &[],
        };
        let w = window("2026-10-01T00:00:00Z", "2026-11-01T00:00:00Z");
        assert_eq!(
            expand(&s, &w, None),
            Err(Unexpandable::Zone("Pacific Standard Time".into()))
        );
        let start = ical("20261001T090000", Some(LA));
        let rules = ["FREQ=FORTNIGHTLY".to_string()];
        let s = Series {
            start: &start,
            rules: &rules,
            ..s
        };
        assert!(matches!(expand(&s, &w, None), Err(Unexpandable::Rule(_))));
    }
}
