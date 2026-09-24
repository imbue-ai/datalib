//! When an event happens, as its source wrote it, and the instant that
//! means. iCalendar has four ways to say a time (a date, UTC, a named
//! zone, floating) and Google a fifth (an offset beside a zone name);
//! they are kept apart until something needs the instant.

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventTime {
    /// An all-day event's day. No zone: the 13th is the 13th everywhere.
    Date(NaiveDate),
    /// A wall-clock time and what it is relative to.
    At { local: NaiveDateTime, zone: Zone },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zone {
    Utc,
    /// An IANA name from a `TZID`, resolved against the zone database.
    Named(String),
    /// A time that came with its offset (Google's `dateTime`), and the
    /// zone name it was written in, if the source said.
    Fixed {
        offset_seconds: i32,
        name: Option<String>,
    },
    /// No zone at all: the same wall-clock time wherever you are.
    Floating,
}

/// Why a time has no instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    /// A `TZID` the zone database does not know (an Outlook name, a
    /// typo). The wall-clock time is still shown.
    UnknownZone(String),
    /// The offset is out of range.
    BadOffset(i32),
}

/// A resolved instant, and whether reading it took a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instant {
    pub at: DateTime<FixedOffset>,
    /// A floating time with no calendar zone to read it in, taken as UTC.
    pub floating_as_utc: bool,
}

impl EventTime {
    /// An iCalendar `DATE` or `DATE-TIME` value with its `TZID`
    /// parameter: `20260913`, `20260913T090000`, `20260913T160000Z`.
    pub fn from_ical(value: &str, tzid: Option<&str>) -> Option<Self> {
        let v = value.trim();
        if v.len() == 8 {
            return NaiveDate::parse_from_str(v, "%Y%m%d")
                .ok()
                .map(EventTime::Date);
        }
        let (body, utc) = match v.strip_suffix('Z').or_else(|| v.strip_suffix('z')) {
            Some(b) => (b, true),
            None => (v, false),
        };
        let local = NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M%S").ok()?;
        let zone = match (utc, tzid) {
            (true, _) => Zone::Utc,
            (false, Some(tz)) if !tz.trim().is_empty() => Zone::Named(tz.trim().to_string()),
            (false, _) => Zone::Floating,
        };
        Some(EventTime::At { local, zone })
    }

    /// Google's `{"date": "2026-09-13"}` or `{"dateTime":
    /// "2026-09-13T09:00:00-07:00", "timeZone": "America/Los_Angeles"}`.
    pub fn from_google(
        date: Option<&str>,
        date_time: Option<&str>,
        zone: Option<&str>,
    ) -> Option<Self> {
        if let Some(dt) = date_time {
            let at = DateTime::parse_from_rfc3339(dt).ok()?;
            return Some(EventTime::At {
                local: at.naive_local(),
                zone: Zone::Fixed {
                    offset_seconds: at.offset().local_minus_utc(),
                    name: zone.filter(|z| !z.is_empty()).map(str::to_string),
                },
            });
        }
        NaiveDate::parse_from_str(date?, "%Y-%m-%d")
            .ok()
            .map(EventTime::Date)
    }

    pub fn is_all_day(&self) -> bool {
        matches!(self, EventTime::Date(_))
    }

    /// The instant this time means. An all-day date is its midnight UTC,
    /// so `after:2026-09-13` finds an event on the 13th whatever zone
    /// the reader is in. A floating time is read in `floating_zone`, the
    /// calendar's own, else as UTC — and says so.
    pub fn instant(&self, floating_zone: Option<&str>) -> Result<Instant, Unresolved> {
        let exact = |at| Instant {
            at,
            floating_as_utc: false,
        };
        match self {
            EventTime::Date(d) => Ok(exact(
                d.and_hms_opt(0, 0, 0)
                    .expect("midnight exists")
                    .and_utc()
                    .fixed_offset(),
            )),
            EventTime::At { local, zone } => match zone {
                Zone::Utc => Ok(exact(local.and_utc().fixed_offset())),
                Zone::Named(name) => in_named_zone(local, name).map(exact),
                Zone::Fixed { offset_seconds, .. } => FixedOffset::east_opt(*offset_seconds)
                    .and_then(|o| o.from_local_datetime(local).single())
                    .map(exact)
                    .ok_or(Unresolved::BadOffset(*offset_seconds)),
                Zone::Floating => match floating_zone.and_then(|z| in_named_zone(local, z).ok()) {
                    Some(at) => Ok(exact(at)),
                    None => Ok(Instant {
                        at: local.and_utc().fixed_offset(),
                        floating_as_utc: true,
                    }),
                },
            },
        }
    }

    /// A stable spelling of this time for keys and matching: the UTC
    /// instant where it resolves (`20260312T160000Z`), the date for an
    /// all-day one, the wall-clock time otherwise. `RECURRENCE-ID`s
    /// written in different zones for the same occurrence spell the same.
    pub fn key(&self, floating_zone: Option<&str>) -> String {
        match self {
            EventTime::Date(d) => d.format("%Y%m%d").to_string(),
            EventTime::At { local, .. } => match self.instant(floating_zone) {
                Ok(i) => {
                    i.at.with_timezone(&Utc)
                        .format("%Y%m%dT%H%M%SZ")
                        .to_string()
                }
                Err(_) => local.format("%Y%m%dT%H%M%S").to_string(),
            },
        }
    }

    /// `Thu 12 Mar 2026`.
    pub fn display_date(&self) -> String {
        match self {
            EventTime::Date(d) => fmt_date(*d),
            EventTime::At { local, .. } => fmt_date(local.date()),
        }
    }

    /// `Thu 12 Mar 2026, 09:00 (America/Los_Angeles)`.
    pub fn display(&self) -> String {
        match self {
            EventTime::Date(d) => fmt_date(*d),
            EventTime::At { local, zone } => format!(
                "{}, {} ({})",
                fmt_date(local.date()),
                local.format("%H:%M"),
                zone_label(zone)
            ),
        }
    }

    /// The zone this time is written in, where it names one.
    pub fn zone_name(&self) -> Option<&str> {
        match self {
            EventTime::At {
                zone: Zone::Named(n),
                ..
            }
            | EventTime::At {
                zone: Zone::Fixed { name: Some(n), .. },
                ..
            } => Some(n),
            _ => None,
        }
    }
}

/// An event's span in one line. An all-day end is exclusive (RFC 5545
/// §3.6.1), so a one-day event ends the next day and shows as one date.
pub fn display_range(start: &EventTime, end: Option<&EventTime>) -> String {
    match (start, end) {
        (EventTime::Date(s), Some(EventTime::Date(e))) => {
            let last = *e - Duration::days(1);
            if last <= *s {
                format!("{} (all day)", fmt_date(*s))
            } else {
                format!("{} – {} (all day)", fmt_date(*s), fmt_date(last))
            }
        }
        (EventTime::Date(s), _) => format!("{} (all day)", fmt_date(*s)),
        (EventTime::At { local: s, zone: sz }, Some(EventTime::At { local: e, zone: ez }))
            if s.date() == e.date() && zone_label(sz) == zone_label(ez) =>
        {
            format!(
                "{}, {}–{} ({})",
                fmt_date(s.date()),
                s.format("%H:%M"),
                e.format("%H:%M"),
                zone_label(sz)
            )
        }
        (s, Some(e)) => format!("{} → {}", s.display(), e.display()),
        (s, None) => s.display(),
    }
}

fn in_named_zone(local: &NaiveDateTime, name: &str) -> Result<DateTime<FixedOffset>, Unresolved> {
    let tz: chrono_tz::Tz = name
        .parse()
        .map_err(|_| Unresolved::UnknownZone(name.to_string()))?;
    // A time in the spring-forward gap does not exist; RFC 5545 §3.3.5
    // reads it as the same span after the gap, which one hour covers for
    // every zone that has one.
    tz.from_local_datetime(local)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(*local + Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.fixed_offset())
        .ok_or_else(|| Unresolved::UnknownZone(name.to_string()))
}

fn fmt_date(d: NaiveDate) -> String {
    d.format("%a %-d %b %Y").to_string()
}

fn zone_label(zone: &Zone) -> String {
    match zone {
        Zone::Utc => "UTC".to_string(),
        Zone::Named(n) => n.clone(),
        Zone::Fixed { name: Some(n), .. } => n.clone(),
        Zone::Fixed {
            offset_seconds,
            name: None,
        } => {
            let sign = if *offset_seconds < 0 { '−' } else { '+' };
            let m = offset_seconds.unsigned_abs() / 60;
            format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
        }
        Zone::Floating => "local time".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(v: &str, tz: Option<&str>) -> EventTime {
        EventTime::from_ical(v, tz).unwrap()
    }

    #[test]
    fn a_named_zone_resolves_across_daylight_saving() {
        let winter = at("20260105T090000", Some("America/Los_Angeles"));
        let summer = at("20260706T090000", Some("America/Los_Angeles"));
        assert_eq!(
            winter.instant(None).unwrap().at.to_rfc3339(),
            "2026-01-05T09:00:00-08:00"
        );
        assert_eq!(
            summer.instant(None).unwrap().at.to_rfc3339(),
            "2026-07-06T09:00:00-07:00"
        );
        assert_eq!(summer.key(None), "20260706T160000Z");
    }

    #[test]
    fn a_time_in_the_spring_gap_moves_past_it() {
        let gap = at("20260308T023000", Some("America/Los_Angeles"));
        assert_eq!(
            gap.instant(None).unwrap().at.to_rfc3339(),
            "2026-03-08T03:30:00-07:00"
        );
    }

    #[test]
    fn floating_reads_in_the_calendar_zone_else_as_utc_and_says_so() {
        let t = at("20260115T200000", None);
        let la = t.instant(Some("America/Los_Angeles")).unwrap();
        assert_eq!(la.at.to_rfc3339(), "2026-01-15T20:00:00-08:00");
        assert!(!la.floating_as_utc);
        let guess = t.instant(None).unwrap();
        assert_eq!(guess.at.to_rfc3339(), "2026-01-15T20:00:00+00:00");
        assert!(guess.floating_as_utc);
    }

    #[test]
    fn an_unknown_zone_is_not_guessed() {
        let t = at("20260115T200000", Some("Pacific Standard Time"));
        assert_eq!(
            t.instant(None),
            Err(Unresolved::UnknownZone("Pacific Standard Time".into()))
        );
        assert_eq!(t.key(None), "20260115T200000");
        assert!(t.display().contains("Pacific Standard Time"));
    }

    #[test]
    fn a_google_time_keeps_its_offset_and_its_zone_name() {
        let t = EventTime::from_google(
            None,
            Some("2026-09-18T19:00:00-07:00"),
            Some("America/Los_Angeles"),
        )
        .unwrap();
        assert_eq!(
            t.instant(None).unwrap().at.to_rfc3339(),
            "2026-09-18T19:00:00-07:00"
        );
        assert_eq!(t.display(), "Fri 18 Sep 2026, 19:00 (America/Los_Angeles)");
        let d = EventTime::from_google(Some("2026-07-13"), None, None).unwrap();
        assert_eq!(
            d,
            EventTime::Date(NaiveDate::from_ymd_opt(2026, 7, 13).unwrap())
        );
    }

    #[test]
    fn ranges_read_naturally() {
        let d = |v| EventTime::from_ical(v, None).unwrap();
        assert_eq!(
            display_range(&d("20260713"), Some(&d("20260718"))),
            "Mon 13 Jul 2026 – Fri 17 Jul 2026 (all day)"
        );
        assert_eq!(
            display_range(&d("20260202"), Some(&d("20260203"))),
            "Mon 2 Feb 2026 (all day)"
        );
        let la = Some("America/Los_Angeles");
        assert_eq!(
            display_range(&at("20260918T190000", la), Some(&at("20260918T220000", la))),
            "Fri 18 Sep 2026, 19:00–22:00 (America/Los_Angeles)"
        );
        assert_eq!(
            display_range(&at("20260922T143000Z", None), None),
            "Tue 22 Sep 2026, 14:30 (UTC)"
        );
    }
}
