//! IANA time zones by name. Every named zone in the workspace is read
//! here, from the tz database bundled into the binary (`jiff-tzdb`), never
//! from the host's: a mac, a Linux container with no `/usr/share/zoneinfo`
//! and Windows then give the same answer, and that answer is as current as
//! the `jiff-tzdb` in `Cargo.lock`.

use chrono::{DateTime, FixedOffset, LocalResult, NaiveDateTime, TimeZone, Timelike, Utc};
use jiff::tz::{AmbiguousOffset, Offset, TimeZoneDatabase};
use std::sync::LazyLock;

static BUNDLED: LazyLock<TimeZoneDatabase> = LazyLock::new(TimeZoneDatabase::bundled);

/// A zone named in the IANA database: `America/Vancouver`, `UTC`.
#[derive(Clone, Debug)]
pub struct NamedZone(jiff::tz::TimeZone);

impl NamedZone {
    /// `None` for a name the database does not know (an Outlook name, a
    /// typo). Case is ignored.
    pub fn get(name: &str) -> Option<Self> {
        BUNDLED.get(name).ok().map(Self)
    }

    /// The wall clock here at `instant`.
    pub fn at<Tz: TimeZone>(&self, instant: &DateTime<Tz>) -> DateTime<FixedOffset> {
        let utc = instant.with_timezone(&Utc);
        let offset = jiff::Timestamp::new(utc.timestamp(), 0)
            .map(|t| self.0.to_offset(t))
            .unwrap_or(Offset::UTC);
        utc.with_timezone(&fixed(offset))
    }

    /// The instants a wall-clock time here can mean: one, two in the hour
    /// the clocks go back (earlier first), none in the hour they skip.
    pub fn from_local(&self, local: &NaiveDateTime) -> LocalResult<DateTime<FixedOffset>> {
        let Some(civil) = civil(local) else {
            return LocalResult::None;
        };
        let with = |o: Offset| {
            fixed(o)
                .from_local_datetime(local)
                .single()
                .map_or(LocalResult::None, LocalResult::Single)
        };
        match self.0.to_ambiguous_timestamp(civil).offset() {
            AmbiguousOffset::Unambiguous { offset } => with(offset),
            AmbiguousOffset::Gap { .. } => LocalResult::None,
            AmbiguousOffset::Fold { before, after } => match (with(before), with(after)) {
                (LocalResult::Single(a), LocalResult::Single(b)) => LocalResult::Ambiguous(a, b),
                _ => LocalResult::None,
            },
        }
    }
}

fn fixed(o: Offset) -> FixedOffset {
    FixedOffset::east_opt(o.seconds()).expect("a tz offset is within a day")
}

fn civil(local: &NaiveDateTime) -> Option<jiff::civil::DateTime> {
    use chrono::Datelike;
    jiff::civil::DateTime::new(
        i16::try_from(local.year()).ok()?,
        local.month() as i8,
        local.day() as i8,
        local.hour() as i8,
        local.minute() as i8,
        local.second() as i8,
        0,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").unwrap()
    }

    /// British Columbia stayed at -07:00 from 2026 on (tzdata 2026b); a
    /// database from before it falls back to -08:00 on 1 Nov 2026.
    #[test]
    fn vancouver_keeps_minus_seven_after_november_2026() {
        let van = NamedZone::get("America/Vancouver").unwrap();
        let at = van.from_local(&local("2026-11-18T11:30")).single().unwrap();
        assert_eq!(at.to_rfc3339(), "2026-11-18T11:30:00-07:00");
        let noon_utc = DateTime::parse_from_rfc3339("2026-12-01T19:00:00Z").unwrap();
        assert_eq!(van.at(&noon_utc).to_rfc3339(), "2026-12-01T12:00:00-07:00");
    }

    #[test]
    fn daylight_rules_run_past_the_listed_transitions() {
        let la = NamedZone::get("America/Los_Angeles").unwrap();
        let at = la.from_local(&local("2061-07-04T09:00")).single().unwrap();
        assert_eq!(at.to_rfc3339(), "2061-07-04T09:00:00-07:00");
    }

    #[test]
    fn the_hour_the_clocks_go_back_has_two_instants_earlier_first() {
        let la = NamedZone::get("America/Los_Angeles").unwrap();
        match la.from_local(&local("2026-11-01T01:30")) {
            LocalResult::Ambiguous(a, b) => {
                assert_eq!(a.to_rfc3339(), "2026-11-01T01:30:00-07:00");
                assert_eq!(b.to_rfc3339(), "2026-11-01T01:30:00-08:00");
            }
            other => panic!("expected two instants, got {other:?}"),
        }
    }

    #[test]
    fn the_hour_the_clocks_skip_has_none() {
        let la = NamedZone::get("America/Los_Angeles").unwrap();
        assert_eq!(la.from_local(&local("2026-03-08T02:30")), LocalResult::None);
    }

    #[test]
    fn an_unknown_name_is_none() {
        assert!(NamedZone::get("Pacific Standard Time").is_none());
        assert!(NamedZone::get("UTC").is_some());
    }
}
