//! Time zones by name. Every named zone in the workspace is read here,
//! from the tz database bundled into the binary (`jiff-tzdb`), never from
//! the host's: a mac, a Linux container with no `/usr/share/zoneinfo` and
//! Windows then give the same answer, and that answer is as current as the
//! `jiff-tzdb` in `Cargo.lock`. Besides IANA's names, two kinds turn up that
//! each mean one thing wherever they appear: Windows' names (`Pacific
//! Standard Time`) and a bare offset (`GMT-0700`).

mod windows;

use chrono::{DateTime, FixedOffset, LocalResult, NaiveDateTime, TimeZone, Timelike, Utc};
use jiff::tz::{AmbiguousOffset, Offset, TimeZoneDatabase};
use std::sync::LazyLock;

static BUNDLED: LazyLock<TimeZoneDatabase> = LazyLock::new(TimeZoneDatabase::bundled);

/// A named zone: `America/Vancouver`, `UTC`, `Pacific Standard Time`,
/// `GMT-0700`.
#[derive(Clone, Debug)]
pub struct NamedZone(jiff::tz::TimeZone);

impl NamedZone {
    /// An IANA name, else a Windows name (CLDR's mapping), else `GMT` or
    /// `UTC` and an offset, which is that offset all year. `None` for
    /// anything else (Outlook's `Customized Time Zone`, a typo), which is
    /// never guessed. Case is ignored.
    pub fn get(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::iana(name)
            .or_else(|| windows(name))
            .or_else(|| offset(name))
    }

    fn iana(name: &str) -> Option<Self> {
        BUNDLED.get(name).ok().map(Self)
    }

    /// A clock that is `offset_seconds` east of UTC all year, with no
    /// daylight saving; `None` past ±25:59:59.
    pub fn fixed(offset_seconds: i32) -> Option<Self> {
        let offset = Offset::from_seconds(offset_seconds).ok()?;
        Some(Self(jiff::tz::TimeZone::fixed(offset)))
    }

    /// The wall clock here at `instant`.
    pub fn at<Tz: TimeZone>(&self, instant: &DateTime<Tz>) -> DateTime<FixedOffset> {
        let utc = instant.with_timezone(&Utc);
        let offset = jiff::Timestamp::new(utc.timestamp(), 0)
            .map(|t| self.0.to_offset(t))
            .unwrap_or(Offset::UTC);
        utc.with_timezone(&fixed(offset))
    }

    /// The offset from UTC here at the instant `ms` (Unix milliseconds),
    /// in seconds east; `None` for an instant chrono cannot represent.
    pub fn offset_secs_at_millis(&self, ms: i64) -> Option<i32> {
        let utc = DateTime::<Utc>::from_timestamp_millis(ms)?;
        Some(self.at(&utc).offset().local_minus_utc())
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

fn windows(name: &str) -> Option<NamedZone> {
    let (_, iana) = windows::WINDOWS
        .iter()
        .find(|(w, _)| w.eq_ignore_ascii_case(name))?;
    NamedZone::iana(iana)
}

/// `GMT-0700`, `UTC+05:30`, `GMT+8`: east of UTC, as ISO 8601 signs it
/// (not POSIX's or `Etc/GMT+7`'s reversed sign).
fn offset(name: &str) -> Option<NamedZone> {
    let prefix = name.get(..3)?;
    if !(prefix.eq_ignore_ascii_case("GMT") || prefix.eq_ignore_ascii_case("UTC")) {
        return None;
    }
    let rest = &name[3..];
    let (sign, digits) = match rest.as_bytes().first()? {
        b'+' => (1, &rest[1..]),
        b'-' => (-1, &rest[1..]),
        _ => return None,
    };
    let digits = digits.replacen(':', "", 1);
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match digits.len() {
        1 | 2 => (digits.as_str(), "0"),
        4 => digits.split_at(2),
        _ => return None,
    };
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 14 || m >= 60 {
        return None;
    }
    NamedZone::fixed(sign * (h * 3600 + m * 60))
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
    fn a_fixed_zone_has_no_daylight_saving() {
        let z = NamedZone::fixed(-7 * 3600).unwrap();
        let winter = z.from_local(&local("2026-01-05T09:00")).single().unwrap();
        let summer = z.from_local(&local("2026-07-06T09:00")).single().unwrap();
        assert_eq!(winter.to_rfc3339(), "2026-01-05T09:00:00-07:00");
        assert_eq!(summer.to_rfc3339(), "2026-07-06T09:00:00-07:00");
        assert!(NamedZone::fixed(30 * 3600).is_none());
    }

    #[test]
    fn an_iana_name_is_found_whatever_its_case() {
        assert!(NamedZone::get("UTC").is_some());
        assert_eq!(
            at("america/los_angeles", "2026-01-05T09:00"),
            "2026-01-05T09:00:00-08:00"
        );
    }

    fn at(name: &str, wall: &str) -> String {
        NamedZone::get(name)
            .unwrap_or_else(|| panic!("{name} did not resolve"))
            .from_local(&local(wall))
            .single()
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn every_windows_name_reaches_a_zone_the_database_holds() {
        let missing: Vec<_> = windows::WINDOWS
            .iter()
            .filter(|(_, iana)| NamedZone::iana(iana).is_none())
            .collect();
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// Outlook writes `TZID=Pacific Standard Time` with the US rules in
    /// its `VTIMEZONE`; calendar events in it had no start.
    #[test]
    fn a_windows_name_keeps_its_daylight_saving() {
        assert_eq!(
            at("Pacific Standard Time", "2026-01-05T09:00"),
            "2026-01-05T09:00:00-08:00"
        );
        assert_eq!(
            at("Pacific Standard Time", "2026-07-06T09:00"),
            "2026-07-06T09:00:00-07:00"
        );
        assert_eq!(
            at("india standard time", "2026-07-06T09:00"),
            "2026-07-06T09:00:00+05:30"
        );
    }

    /// `TZID=GMT-0700` comes with a `VTIMEZONE` that is -07:00 all year;
    /// calendar events in it had no start.
    #[test]
    fn a_gmt_offset_is_that_offset_all_year() {
        assert_eq!(
            at("GMT-0700", "2026-01-05T09:00"),
            "2026-01-05T09:00:00-07:00"
        );
        assert_eq!(
            at("GMT-0700", "2026-07-06T09:00"),
            "2026-07-06T09:00:00-07:00"
        );
        assert_eq!(
            at("UTC+05:30", "2026-07-06T09:00"),
            "2026-07-06T09:00:00+05:30"
        );
        assert_eq!(at("GMT+8", "2026-07-06T09:00"), "2026-07-06T09:00:00+08:00");
    }

    #[test]
    fn anything_else_is_unknown() {
        for name in [
            "Customized Time Zone",
            "GMT-0760",
            "GMT+1500",
            "GMT 0700",
            "GMT-07000",
            "GMT-07:0",
        ] {
            assert!(NamedZone::get(name).is_none(), "{name}");
        }
    }
}
