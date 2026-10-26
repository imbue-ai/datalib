//! How a config says where a source's time window starts and ends: the
//! one grammar every `since` / `until` shares. A bound is a day
//! (`2026-09-01`), an instant (`2026-09-01T09:00:00-07:00`), or an offset
//! from the run's `now` (`-30d`, `+21d`, `-12h`, `-2w`). An offset is
//! resolved afresh each run against `DATALIB_DAG_NOW`, so the window
//! slides; what a source does with records that slide out of it is the
//! source's to say (docs/dev/config_model.md, "Time windows").

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, TimeZone, Utc};

use crate::IsoOffsetTimestamp;

/// One end of a window, as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBound {
    /// `YYYY-MM-DD`. As an instant it is midnight UTC; as a day, itself.
    Day(NaiveDate),
    /// RFC 3339, its offset kept. As a day, its date in that offset.
    Instant(DateTime<FixedOffset>),
    /// `-30d`, `+21d`: this far from the run's `now`. As a day, the date
    /// it lands on in `now`'s offset.
    Relative(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    #[error(
        "{input:?} is not a time: write a day (2026-09-01), an RFC 3339 instant \
         (2026-09-01T09:00:00-07:00), or an offset from now with its sign (-30d, +21d; \
         units h, d, w)"
    )]
    Unreadable { input: String },
    #[error("`until` ({until}) is before `since` ({since})")]
    Reversed { since: String, until: String },
}

impl TimeBound {
    pub fn parse(s: &str) -> Result<Self, WindowError> {
        let unreadable = || WindowError::Unreadable {
            input: s.to_string(),
        };
        if let Some(rest) = s.strip_prefix('-').or_else(|| s.strip_prefix('+')) {
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let unit = rest.chars().last().ok_or_else(unreadable)?;
            let digits = &rest[..rest.len() - unit.len_utf8()];
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(unreadable());
            }
            let n: i64 = digits.parse().map_err(|_| unreadable())?;
            let span = match unit {
                'h' => Duration::try_hours(n),
                'd' => Duration::try_days(n),
                'w' => Duration::try_weeks(n),
                _ => None,
            }
            .ok_or_else(unreadable)?;
            return Ok(Self::Relative(span * sign));
        }
        if let Ok(t) = DateTime::parse_from_rfc3339(s) {
            return Ok(Self::Instant(t));
        }
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(Self::Day)
            .map_err(|_| unreadable())
    }

    /// `s` as a source that reads a day or an RFC 3339 instant takes it:
    /// as written, or an offset's instant on this run.
    pub fn absolute(s: &str, now: &IsoOffsetTimestamp) -> Result<String, WindowError> {
        Ok(match Self::parse(s)? {
            b @ Self::Relative(_) => b.instant(now).to_rfc3339(),
            Self::Day(_) | Self::Instant(_) => s.to_string(),
        })
    }

    pub fn instant(&self, now: &IsoOffsetTimestamp) -> IsoOffsetTimestamp {
        match *self {
            Self::Day(d) => Utc
                .from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight exists"))
                .into(),
            Self::Instant(t) => t.into(),
            Self::Relative(span) => (now.inner() + span).into(),
        }
    }

    pub fn day(&self, now: &IsoOffsetTimestamp) -> NaiveDate {
        match *self {
            Self::Day(d) => d,
            Self::Instant(t) => t.date_naive(),
            Self::Relative(span) => (now.inner() + span).date_naive(),
        }
    }

    /// Whether `self` comes after `other` whatever the clock says; `false`
    /// when only a clock could tell (an offset against a date).
    fn surely_after(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Relative(a), Self::Relative(b)) => a > b,
            (Self::Relative(_), _) | (_, Self::Relative(_)) => false,
            (Self::Day(a), Self::Day(b)) => a > b,
            // Any `now` will do: neither end reads it.
            (a, b) => {
                let now = IsoOffsetTimestamp::from(DateTime::<Utc>::UNIX_EPOCH);
                a.instant(&now).inner() > b.instant(&now).inner()
            }
        }
    }
}

/// A source's `since` and `until`, either one open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeWindow {
    pub since: Option<TimeBound>,
    pub until: Option<TimeBound>,
}

impl TimeWindow {
    /// Both ends read, and `until` not before `since` wherever that can
    /// be told without a clock; [`Self::days`] and [`Self::instants`]
    /// tell the rest each run.
    pub fn parse(since: Option<&str>, until: Option<&str>) -> Result<Self, WindowError> {
        let w = Self {
            since: since.map(TimeBound::parse).transpose()?,
            until: until.map(TimeBound::parse).transpose()?,
        };
        if let (Some(s), Some(u)) = (&w.since, &w.until) {
            if s.surely_after(u) {
                return Err(reversed(since, until));
            }
        }
        Ok(w)
    }

    /// [`Self::parse`] for a config's `validate`, its message naming the
    /// key as `<table>.since`.
    pub fn check(table: &str, since: Option<&str>, until: Option<&str>) -> Result<Self, String> {
        for (key, v) in [("since", since), ("until", until)] {
            if let Some(v) = v {
                TimeBound::parse(v).map_err(|e| format!("`{table}.{key}`: {e}"))?;
            }
        }
        Self::parse(since, until).map_err(|e| format!("`{table}`: {e}"))
    }

    pub fn is_open(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    /// Both ends as instants on this run.
    pub fn instants(
        &self,
        now: &IsoOffsetTimestamp,
    ) -> Result<(Option<IsoOffsetTimestamp>, Option<IsoOffsetTimestamp>), WindowError> {
        let since = self.since.map(|b| b.instant(now));
        let until = self.until.map(|b| b.instant(now));
        if let (Some(s), Some(u)) = (&since, &until) {
            if s.inner() > u.inner() {
                return Err(reversed(Some(&s.to_rfc3339()), Some(&u.to_rfc3339())));
            }
        }
        Ok((since, until))
    }

    /// Both ends as days on this run, `until` included.
    pub fn days(
        &self,
        now: &IsoOffsetTimestamp,
    ) -> Result<(Option<NaiveDate>, Option<NaiveDate>), WindowError> {
        let since = self.since.map(|b| b.day(now));
        let until = self.until.map(|b| b.day(now));
        if let (Some(s), Some(u)) = (since, until) {
            if s > u {
                return Err(reversed(Some(&s.to_string()), Some(&u.to_string())));
            }
        }
        Ok((since, until))
    }
}

fn reversed(since: Option<&str>, until: Option<&str>) -> WindowError {
    WindowError::Reversed {
        since: since.unwrap_or_default().to_string(),
        until: until.unwrap_or_default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> IsoOffsetTimestamp {
        crate::parse_strict("2026-10-26T15:30:00-07:00").unwrap()
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn a_day_is_midnight_utc_and_itself() {
        let b = TimeBound::parse("2026-09-01").unwrap();
        assert_eq!(b.instant(&now()).to_rfc3339(), "2026-09-01T00:00:00+00:00");
        assert_eq!(b.day(&now()), day("2026-09-01"));
    }

    #[test]
    fn an_instant_keeps_its_offset_and_its_own_date() {
        let b = TimeBound::parse("2026-09-01T23:30:00-07:00").unwrap();
        assert_eq!(b.instant(&now()).to_rfc3339(), "2026-09-01T23:30:00-07:00");
        assert_eq!(
            b.day(&now()),
            day("2026-09-01"),
            "the date where it was written"
        );
    }

    #[test]
    fn an_offset_counts_from_now_in_each_unit() {
        let at = |s: &str| TimeBound::parse(s).unwrap().instant(&now()).to_rfc3339();
        assert_eq!(at("-30d"), "2026-09-26T15:30:00-07:00");
        assert_eq!(at("+21d"), "2026-11-16T15:30:00-07:00");
        assert_eq!(at("-12h"), "2026-10-26T03:30:00-07:00");
        assert_eq!(at("-2w"), "2026-10-12T15:30:00-07:00");
        assert_eq!(at("+0d"), now().to_rfc3339());
    }

    /// 15:30 at -07:00 is already the 27th in UTC; the day an offset
    /// lands on is read in the run's own offset, as `today` is.
    #[test]
    fn an_offsets_day_is_read_in_nows_offset() {
        let b = TimeBound::parse("-1d").unwrap();
        let late = crate::parse_strict("2026-10-26T20:00:00-07:00").unwrap();
        assert_eq!(b.day(&late), day("2026-10-25"));
    }

    #[test]
    fn what_is_not_a_bound_is_refused() {
        for bad in [
            "",
            "30d",
            "-30",
            "-d",
            "-30m",
            "-30D",
            "- 30d",
            "-3.5d",
            "2026/09/01",
            "yesterday",
            "-99999999999999w",
            "2026-09-01T09:00:00",
        ] {
            assert!(
                matches!(TimeBound::parse(bad), Err(WindowError::Unreadable { .. })),
                "{bad:?}"
            );
        }
    }

    /// Every value that loaded before the grammar still loads, to the
    /// same instant: Slack, Claude and ChatGPT took a day or RFC 3339,
    /// the calendar and Garmin a day, and Slack's "before 1970 is
    /// everything" stays a day.
    #[test]
    fn values_written_before_the_grammar_mean_what_they_meant() {
        for (s, at) in [
            ("2024-01-01", "2024-01-01T00:00:00+00:00"),
            ("1900-01-01", "1900-01-01T00:00:00+00:00"),
            ("2024-03-20T18:28:51Z", "2024-03-20T18:28:51+00:00"),
            ("2024-03-20T18:28:51+02:00", "2024-03-20T18:28:51+02:00"),
        ] {
            assert_eq!(
                TimeBound::parse(s).unwrap().instant(&now()).to_rfc3339(),
                at
            );
        }
    }

    #[test]
    fn a_window_refuses_until_before_since_when_no_clock_is_needed() {
        let parse = |s, u| TimeWindow::parse(Some(s), Some(u));
        assert!(parse("2026-09-02", "2026-09-01").is_err());
        assert!(parse("-1d", "-2d").is_err());
        assert!(parse("2026-09-01T12:00:00Z", "2026-09-01").is_err());
        assert!(
            parse("2026-09-01", "2026-09-01").is_ok(),
            "one day, both ends included"
        );
        assert!(parse("-2d", "+21d").is_ok());
        assert!(parse("2099-01-01", "-1d").is_ok(), "only a run can tell");
    }

    #[test]
    fn a_run_refuses_a_window_that_resolves_reversed() {
        let w = TimeWindow::parse(Some("2099-01-01"), Some("-1d")).unwrap();
        assert!(matches!(w.days(&now()), Err(WindowError::Reversed { .. })));
        assert!(matches!(
            w.instants(&now()),
            Err(WindowError::Reversed { .. })
        ));
    }

    #[test]
    fn a_window_resolves_both_ends_against_one_now() {
        let w = TimeWindow::parse(Some("-2d"), Some("+21d")).unwrap();
        assert_eq!(
            w.days(&now()).unwrap(),
            (Some(day("2026-10-24")), Some(day("2026-11-16")))
        );
        assert!(TimeWindow::parse(None, None).unwrap().is_open());
    }

    #[test]
    fn absolute_keeps_what_was_written_and_resolves_an_offset() {
        let abs = |s| TimeBound::absolute(s, &now()).unwrap();
        assert_eq!(abs("2024-01-01"), "2024-01-01");
        assert_eq!(abs("2024-03-20T18:28:51Z"), "2024-03-20T18:28:51Z");
        assert_eq!(abs("-30d"), "2026-09-26T15:30:00-07:00");
        assert!(TimeBound::absolute("soon", &now()).is_err());
    }

    #[test]
    fn a_configs_check_names_the_key() {
        let err = TimeWindow::check("api", Some("-30"), None).unwrap_err();
        assert!(
            err.starts_with("`api.since`: \"-30\" is not a time"),
            "{err}"
        );
        let err = TimeWindow::check("google", Some("-1d"), Some("-2d")).unwrap_err();
        assert_eq!(err, "`google`: `until` (-2d) is before `since` (-1d)");
    }
}
